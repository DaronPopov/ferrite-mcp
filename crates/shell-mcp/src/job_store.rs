//! JobStore — spawns, tracks, and persists background processes.
//!
//! Each Job has:
//!   - stdout / stderr drained by background reader threads into in-memory buffers
//!   - a combined log written to ~/.local/share/ferrite/logs/{job_id}.log (persists restarts)
//!   - a waiter thread that marks the job Done on exit and triggers a session save
//!   - cursor-based incremental reads for bg_status

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::persist::{log_path_for, JobSnapshot, Persistence, SessionData};
use crate::tools::state::lock_mutex;

// PTY support
use std::os::unix::io::FromRawFd;
use std::os::unix::process::CommandExt;

// ── JobStatus ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum JobStatus {
    Running,
    Done(i32),
    Killed,
    /// Created via bg_attach — monitored via /proc, no live output capture.
    Attached,
}

impl JobStatus {
    pub fn label(&self) -> &'static str {
        match self {
            JobStatus::Running | JobStatus::Attached => "running",
            JobStatus::Done(_) => "done",
            JobStatus::Killed => "killed",
        }
    }
    pub fn exit_code(&self) -> Option<i32> {
        if let JobStatus::Done(c) = self {
            Some(*c)
        } else {
            None
        }
    }
    pub fn is_terminal(&self) -> bool {
        matches!(self, JobStatus::Done(_) | JobStatus::Killed)
    }
    fn as_snapshot_str(&self) -> &'static str {
        match self {
            JobStatus::Running => "running",
            JobStatus::Done(_) => "done",
            JobStatus::Killed => "killed",
            JobStatus::Attached => "attached",
        }
    }
}

// ── Job ───────────────────────────────────────────────────────────────────────

pub struct Job {
    pub job_id: String,
    pub pid: u32,
    pub label: String,
    pub cmd: String,
    pub cwd: PathBuf,
    pub status: Arc<Mutex<JobStatus>>,
    pub stdout_buf: Arc<Mutex<Vec<u8>>>,
    pub stderr_buf: Arc<Mutex<Vec<u8>>>,
    /// Persistent combined log file — survives ferrite restarts.
    pub log_path: PathBuf,
    /// Cursor for incremental bg_status reads.
    pub stdout_cursor: Mutex<usize>,
    pub stderr_cursor: Mutex<usize>,
    /// Unix timestamp of spawn time — used for elapsed_secs and persistence.
    pub started_secs: u64,
    /// PTY master file — Some when spawned with spawn_pty(). Write here to send input.
    pub stdin_tx: Option<Arc<Mutex<std::fs::File>>>,
    /// True when this job was spawned with a PTY.
    pub is_pty: bool,
}

impl Job {
    pub fn elapsed_secs(&self) -> f64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        (now.saturating_sub(self.started_secs)) as f64
    }

    pub fn drain_new_stdout(&self) -> Vec<u8> {
        let buf = lock_mutex(&self.stdout_buf, "job stdout buffer");
        let mut cur = lock_mutex(&self.stdout_cursor, "job stdout cursor");
        let slice = buf[*cur..].to_vec();
        *cur = buf.len();
        slice
    }

    pub fn drain_new_stderr(&self) -> Vec<u8> {
        let buf = lock_mutex(&self.stderr_buf, "job stderr buffer");
        let mut cur = lock_mutex(&self.stderr_cursor, "job stderr cursor");
        let slice = buf[*cur..].to_vec();
        *cur = buf.len();
        slice
    }

    pub fn full_stdout(&self) -> Vec<u8> {
        lock_mutex(&self.stdout_buf, "job stdout buffer").clone()
    }
    pub fn full_stderr(&self) -> Vec<u8> {
        lock_mutex(&self.stderr_buf, "job stderr buffer").clone()
    }
    pub fn stdout_bytes(&self) -> usize {
        lock_mutex(&self.stdout_buf, "job stdout buffer").len()
    }
    pub fn stderr_bytes(&self) -> usize {
        lock_mutex(&self.stderr_buf, "job stderr buffer").len()
    }
    pub fn current_status(&self) -> JobStatus {
        lock_mutex(&self.status, "job status").clone()
    }

    /// Block until terminal state or timeout. Returns true if completed.
    pub fn wait(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.current_status().is_terminal() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(150));
        }
    }

    pub fn to_snapshot(&self) -> JobSnapshot {
        let status = self.current_status();
        JobSnapshot {
            job_id: self.job_id.clone(),
            pid: self.pid,
            label: self.label.clone(),
            cmd: self.cmd.clone(),
            cwd: self.cwd.display().to_string(),
            started_secs: self.started_secs,
            status: status.as_snapshot_str().to_string(),
            exit_code: status.exit_code(),
            log_path: self.log_path.display().to_string(),
        }
    }
}

// ── JobStore ──────────────────────────────────────────────────────────────────

pub struct JobStore {
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    counter: AtomicUsize,
    persistence: Arc<Persistence>,
}

impl JobStore {
    #[allow(dead_code)]
    pub fn new(persistence: Arc<Persistence>) -> Self {
        Self {
            jobs: Mutex::new(HashMap::new()),
            counter: AtomicUsize::new(1),
            persistence,
        }
    }

    /// Return the Unix timestamp of the most recent system boot.
    /// Returns 0 on failure (safe: all jobs will appear pre-boot).
    #[cfg(target_os = "linux")]
    fn boot_time_secs() -> u64 {
        let uptime_secs = std::fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
            .unwrap_or(0.0) as u64;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        now.saturating_sub(uptime_secs)
    }

    /// Return the Unix timestamp of the most recent system boot via sysctl.
    /// Returns 0 on failure (safe: all jobs will appear pre-boot).
    #[cfg(not(target_os = "linux"))]
    fn boot_time_secs() -> u64 {
        // sysctl kern.boottime → "{ sec = 1234567890, usec = 0 } ..."
        std::process::Command::new("sysctl")
            .args(["-n", "kern.boottime"])
            .output()
            .ok()
            .and_then(|o| {
                let s = String::from_utf8_lossy(&o.stdout).into_owned();
                s.find("sec = ")
                    .and_then(|i| s[i + 6..].split([',', '}']).next()?.trim().parse().ok())
            })
            .unwrap_or(0)
    }

    /// Restore jobs from the last saved session.
    /// Running jobs that are still alive in /proc AND started after the last
    /// boot are re-attached.  Jobs that pre-date the current boot are always
    /// marked Done(-1) — their PIDs may have been recycled by the OS.
    pub fn restore(persistence: Arc<Persistence>) -> Self {
        let data = persistence.load();
        let counter_start = data.counter.max(1);

        let store = Self {
            jobs: Mutex::new(HashMap::new()),
            counter: AtomicUsize::new(counter_start),
            persistence: Arc::clone(&persistence),
        };

        let boot_time = Self::boot_time_secs();

        for snap in data.jobs {
            let was_running = snap.status == "running" || snap.status == "attached";

            let (initial_status, should_poll) = if was_running {
                // If the job started before this boot, the PID is stale —
                // the OS may have recycled it for a completely unrelated process.
                let started_this_boot = snap.started_secs >= boot_time;
                let alive = started_this_boot && pid_alive(snap.pid);
                if alive {
                    (JobStatus::Attached, true)
                } else {
                    (JobStatus::Done(-1), false)
                }
            } else if snap.status == "killed" {
                (JobStatus::Killed, false)
            } else {
                (JobStatus::Done(snap.exit_code.unwrap_or(0)), false)
            };

            let status_arc = Arc::new(Mutex::new(initial_status));

            if should_poll {
                let status_t = Arc::clone(&status_arc);
                let pid = snap.pid;
                thread::spawn(move || loop {
                    thread::sleep(Duration::from_secs(1));
                    {
                        let s = lock_mutex(&status_t, "restored job status");
                        if s.is_terminal() {
                            break;
                        }
                    }
                    if !pid_alive(pid) {
                        *lock_mutex(&status_t, "restored job status") = JobStatus::Done(-1);
                        break;
                    }
                });
            }

            let job = Arc::new(Job {
                job_id: snap.job_id.clone(),
                pid: snap.pid,
                label: snap.label,
                cmd: snap.cmd,
                cwd: PathBuf::from(snap.cwd),
                status: status_arc,
                stdout_buf: Arc::new(Mutex::new(Vec::new())),
                stderr_buf: Arc::new(Mutex::new(Vec::new())),
                log_path: PathBuf::from(snap.log_path),
                stdout_cursor: Mutex::new(0),
                stderr_cursor: Mutex::new(0),
                started_secs: snap.started_secs,
                stdin_tx: None, // PTY master not restored across restarts
                is_pty: false,
            });

            lock_mutex(&store.jobs, "job store map").insert(snap.job_id, job);
        }

        store
    }

    fn next_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("j{n:04}")
    }

    /// Snapshot all jobs and save to disk. Called after mutations.
    pub fn persist_all(&self) {
        let jobs = lock_mutex(&self.jobs, "job store map");
        let counter = self.counter.load(Ordering::Relaxed);
        let snapshots: Vec<JobSnapshot> = jobs.values().map(|j| j.to_snapshot()).collect();
        self.persistence.save(&SessionData {
            version: 1,
            counter,
            jobs: snapshots,
        });
    }

    // ── spawn ─────────────────────────────────────────────────────────────────

    pub fn spawn(
        &self,
        cmd: &str,
        cwd: PathBuf,
        label: Option<&str>,
        extra_env: Vec<(String, String)>,
    ) -> Result<Arc<Job>, String> {
        let job_id = self.next_id();
        let label = label.unwrap_or(cmd).to_string();
        let log_path = log_path_for(&job_id);

        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(&cwd)
            .envs(extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn failed: {e}"))?;

        let pid = child.id();
        let stdout_pipe = child.stdout.take().unwrap();
        let stderr_pipe = child.stderr.take().unwrap();

        let started_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let status = Arc::new(Mutex::new(JobStatus::Running));
        let stdout_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let stderr_buf = Arc::new(Mutex::new(Vec::<u8>::new()));

        // Shared log file — both stdout and stderr threads write here.
        let log_file: Arc<Mutex<Option<fs::File>>> = Arc::new(Mutex::new(
            fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&log_path)
                .ok(),
        ));

        // ── stdout reader thread ──────────────────────────────────────────────
        {
            let buf_t = Arc::clone(&stdout_buf);
            let log_t = Arc::clone(&log_file);
            thread::spawn(move || {
                let mut reader = std::io::BufReader::new(stdout_pipe);
                let mut chunk = [0u8; 8192];
                loop {
                    match reader.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let data = &chunk[..n];
                            lock_mutex(&buf_t, "job stdout buffer").extend_from_slice(data);
                            if let Some(ref mut f) = *lock_mutex(&log_t, "job log file") {
                                let _ = f.write_all(data);
                                let _ = f.flush();
                            }
                        }
                    }
                }
            });
        }

        // ── stderr reader thread ──────────────────────────────────────────────
        {
            let buf_t = Arc::clone(&stderr_buf);
            let log_t = Arc::clone(&log_file);
            thread::spawn(move || {
                let mut reader = std::io::BufReader::new(stderr_pipe);
                let mut chunk = [0u8; 8192];
                loop {
                    match reader.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let data = &chunk[..n];
                            lock_mutex(&buf_t, "job stderr buffer").extend_from_slice(data);
                            if let Some(ref mut f) = *lock_mutex(&log_t, "job log file") {
                                let _ = f.write_all(data);
                                let _ = f.flush();
                            }
                        }
                    }
                }
            });
        }

        // ── waiter thread — marks job done, writes sentinel, triggers persist ──
        {
            let status_t = Arc::clone(&status);
            let log_path_t = log_path.clone();
            thread::spawn(move || {
                let code = match child.wait() {
                    Ok(exit) => exit.code().unwrap_or(-1),
                    Err(_) => -1,
                };
                *lock_mutex(&status_t, "job status") = JobStatus::Done(code);
                // Write FERRITE_DONE sentinel so colorized_watch_cmd banner triggers.
                // Note: full persist_all is triggered by the next tool call that
                // reads this job's status. See query.rs bg_status / bg_list.
                let sentinel = format!("FERRITE_DONE:{code}\n");
                if let Ok(mut f) = fs::OpenOptions::new().append(true).open(&log_path_t) {
                    let _ = f.write_all(sentinel.as_bytes());
                }
            });
        }

        let job = Arc::new(Job {
            job_id: job_id.clone(),
            pid,
            label,
            cmd: cmd.to_string(),
            cwd,
            status,
            stdout_buf,
            stderr_buf,
            log_path,
            stdout_cursor: Mutex::new(0),
            stderr_cursor: Mutex::new(0),
            started_secs,
            stdin_tx: None,
            is_pty: false,
        });

        lock_mutex(&self.jobs, "job store map").insert(job_id, Arc::clone(&job));
        self.persist_all();
        Ok(job)
    }

    // ── spawn_pty ─────────────────────────────────────────────────────────────

    /// Spawn a command in a pseudo-terminal. Stdout+stderr come through the PTY
    /// master (merged). Write to the returned job's stdin_tx to send input.
    pub fn spawn_pty(
        &self,
        cmd: &str,
        cwd: PathBuf,
        label: Option<&str>,
        extra_env: Vec<(String, String)>,
    ) -> Result<Arc<Job>, String> {
        let job_id = self.next_id();
        let label = label.unwrap_or(cmd).to_string();
        let log_path = crate::persist::log_path_for(&job_id);

        // Open PTY master + slave via POSIX API (no libutil needed).
        let (master_raw, slave_raw) = unsafe { open_pty() }?;

        // Dup slave for stdin/stdout/stderr; Stdio::from_raw_fd takes ownership.
        let slave_in = unsafe { libc::dup(slave_raw) };
        let slave_out = unsafe { libc::dup(slave_raw) };
        let slave_err = unsafe { libc::dup(slave_raw) };
        if slave_in < 0 || slave_out < 0 || slave_err < 0 {
            unsafe {
                libc::close(master_raw);
                libc::close(slave_raw);
            }
            return Err(format!(
                "spawn_pty: dup slave fd: {}",
                std::io::Error::last_os_error()
            ));
        }
        // Close the original slave in the parent after dup.
        unsafe {
            libc::close(slave_raw);
        }

        let mut cmd_builder = Command::new("/bin/sh");
        cmd_builder
            .arg("-c")
            .arg(cmd)
            .current_dir(&cwd)
            .envs(extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(unsafe { Stdio::from_raw_fd(slave_in) })
            .stdout(unsafe { Stdio::from_raw_fd(slave_out) })
            .stderr(unsafe { Stdio::from_raw_fd(slave_err) });

        unsafe {
            cmd_builder.pre_exec(move || {
                // New session — fd 0 becomes controlling terminal.
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0i32);
                Ok(())
            });
        }

        let child = cmd_builder
            .spawn()
            .map_err(|e| format!("spawn_pty failed: {e}"))?;
        let pid = child.id();

        let started_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let status = Arc::new(Mutex::new(JobStatus::Running));
        let stdout_buf = Arc::new(Mutex::new(Vec::<u8>::new()));

        // PTY master: clone for reading output, keep original for writing input.
        let master_read = unsafe { std::fs::File::from_raw_fd(master_raw) };
        let master_write = master_read
            .try_clone()
            .map_err(|e| format!("clone pty master: {e}"))?;
        let master_tx = Arc::new(Mutex::new(master_write));

        let log_file: Arc<Mutex<Option<fs::File>>> = Arc::new(Mutex::new(
            fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&log_path)
                .ok(),
        ));

        // ── PTY reader thread — reads master, feeds stdout_buf + log ─────────
        {
            let buf_t = Arc::clone(&stdout_buf);
            let log_t = Arc::clone(&log_file);
            thread::spawn(move || {
                use std::io::Read;
                let mut reader = std::io::BufReader::new(master_read);
                let mut chunk = [0u8; 8192];
                loop {
                    match reader.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let data = &chunk[..n];
                            lock_mutex(&buf_t, "pty stdout buffer").extend_from_slice(data);
                            if let Some(ref mut f) = *lock_mutex(&log_t, "job log file") {
                                let _ = f.write_all(data);
                                let _ = f.flush();
                            }
                        }
                    }
                }
            });
        }

        // ── waiter thread ────────────────────────────────────────────────────
        {
            let status_t = Arc::clone(&status);
            let log_path_t = log_path.clone();
            let mut child = child;
            thread::spawn(move || {
                let code = match child.wait() {
                    Ok(exit) => exit.code().unwrap_or(-1),
                    Err(_) => -1,
                };
                *lock_mutex(&status_t, "pty job status") = JobStatus::Done(code);
                // Write FERRITE_DONE sentinel so colorized_watch_cmd banner triggers.
                let sentinel = format!("FERRITE_DONE:{code}\n");
                if let Ok(mut f) = fs::OpenOptions::new().append(true).open(&log_path_t) {
                    let _ = f.write_all(sentinel.as_bytes());
                }
            });
        }

        let job = Arc::new(Job {
            job_id: job_id.clone(),
            pid,
            label,
            cmd: cmd.to_string(),
            cwd,
            status,
            stdout_buf,
            stderr_buf: Arc::new(Mutex::new(Vec::new())), // PTY merges stderr into stdout
            log_path,
            stdout_cursor: Mutex::new(0),
            stderr_cursor: Mutex::new(0),
            started_secs,
            stdin_tx: Some(master_tx),
            is_pty: true,
        });

        lock_mutex(&self.jobs, "job store map").insert(job_id, Arc::clone(&job));
        self.persist_all();
        Ok(job)
    }

    // ── attach ────────────────────────────────────────────────────────────────

    pub fn attach(&self, pid: u32, label: Option<&str>) -> Result<Arc<Job>, String> {
        if !pid_alive(pid) {
            return Err(format!("PID {pid} not found — is it still running?"));
        }

        let job_id = self.next_id();
        let label = label.unwrap_or(&format!("pid {pid}")).to_string();
        let log_path = log_path_for(&job_id);

        let started_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let status = Arc::new(Mutex::new(JobStatus::Attached));
        {
            let status_t = Arc::clone(&status);
            thread::spawn(move || loop {
                thread::sleep(Duration::from_secs(1));
                {
                    let s = lock_mutex(&status_t, "attached job status");
                    if s.is_terminal() {
                        break;
                    }
                }
                if !pid_alive(pid) {
                    *lock_mutex(&status_t, "attached job status") = JobStatus::Done(-1);
                    break;
                }
            });
        }

        let job = Arc::new(Job {
            job_id: job_id.clone(),
            pid,
            label,
            cmd: format!("<attached pid {pid}>"),
            cwd: PathBuf::from("/"),
            status,
            stdout_buf: Arc::new(Mutex::new(Vec::new())),
            stderr_buf: Arc::new(Mutex::new(Vec::new())),
            log_path,
            stdout_cursor: Mutex::new(0),
            stderr_cursor: Mutex::new(0),
            started_secs,
            stdin_tx: None,
            is_pty: false,
        });

        lock_mutex(&self.jobs, "job store map").insert(job_id, Arc::clone(&job));
        self.persist_all();
        Ok(job)
    }

    // ── lookup ────────────────────────────────────────────────────────────────

    pub fn get(&self, job_id: &str) -> Option<Arc<Job>> {
        lock_mutex(&self.jobs, "job store map").get(job_id).cloned()
    }

    pub fn all(&self) -> Vec<Arc<Job>> {
        let mut jobs: Vec<Arc<Job>> = lock_mutex(&self.jobs, "job store map")
            .values()
            .cloned()
            .collect();
        jobs.sort_by_key(|j| j.started_secs);
        jobs
    }

    pub fn stats(&self) -> JobStoreStats {
        let jobs: Vec<Arc<Job>> = lock_mutex(&self.jobs, "job store map")
            .values()
            .cloned()
            .collect();
        let mut running = 0usize;
        let mut attached = 0usize;
        let mut done = 0usize;
        let mut killed = 0usize;
        let mut stdout_bytes = 0usize;
        let mut stderr_bytes = 0usize;

        for job in &jobs {
            match job.current_status() {
                JobStatus::Running => running += 1,
                JobStatus::Attached => attached += 1,
                JobStatus::Done(_) => done += 1,
                JobStatus::Killed => killed += 1,
            }
            stdout_bytes += job.stdout_bytes();
            stderr_bytes += job.stderr_bytes();
        }

        JobStoreStats {
            total_jobs: jobs.len(),
            running_jobs: running,
            attached_jobs: attached,
            done_jobs: done,
            killed_jobs: killed,
            stdout_bytes,
            stderr_bytes,
            buffered_bytes: stdout_bytes + stderr_bytes,
        }
    }

    #[allow(dead_code)]
    pub fn remove_job(&self, job_id: &str) -> bool {
        let removed = lock_mutex(&self.jobs, "job store map")
            .remove(job_id)
            .is_some();
        if removed {
            self.persist_all();
        }
        removed
    }
}

#[derive(Debug, Clone)]
pub struct JobStoreStats {
    pub total_jobs: usize,
    pub running_jobs: usize,
    pub attached_jobs: usize,
    pub done_jobs: usize,
    pub killed_jobs: usize,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    pub buffered_bytes: usize,
}

// ── Process alive check ───────────────────────────────────────────────────────

/// Returns true if the process with the given PID is still running.
fn pid_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
}

// ── PTY helpers ───────────────────────────────────────────────────────────────

/// Open a pseudo-terminal pair using POSIX APIs (no libutil/openpty needed).
/// Returns (master_fd, slave_fd).
unsafe fn open_pty() -> Result<(i32, i32), String> {
    let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
    if master < 0 {
        return Err(format!("posix_openpt: {}", std::io::Error::last_os_error()));
    }
    if libc::grantpt(master) < 0 {
        libc::close(master);
        return Err(format!("grantpt: {}", std::io::Error::last_os_error()));
    }
    if libc::unlockpt(master) < 0 {
        libc::close(master);
        return Err(format!("unlockpt: {}", std::io::Error::last_os_error()));
    }

    // Get the PTY slave path (ptsname_r on Linux; ptsname on macOS).
    #[cfg(target_os = "linux")]
    let slave_cstr = {
        let mut buf = [0 as libc::c_char; 256];
        if libc::ptsname_r(master, buf.as_mut_ptr(), buf.len()) < 0 {
            libc::close(master);
            return Err(format!("ptsname_r: {}", std::io::Error::last_os_error()));
        }
        std::ffi::CStr::from_ptr(buf.as_ptr()).to_owned()
    };
    #[cfg(not(target_os = "linux"))]
    let slave_cstr = {
        let ptr = libc::ptsname(master);
        if ptr.is_null() {
            libc::close(master);
            return Err(format!("ptsname: {}", std::io::Error::last_os_error()));
        }
        std::ffi::CStr::from_ptr(ptr).to_owned()
    };
    let slave_path = slave_cstr.as_c_str();
    let slave = libc::open(slave_path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
    if slave < 0 {
        libc::close(master);
        return Err(format!(
            "open slave pty: {}",
            std::io::Error::last_os_error()
        ));
    }

    Ok((master, slave))
}
