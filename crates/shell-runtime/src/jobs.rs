//! Job control: job table, foreground/background, wait.

use std::collections::HashMap;

use shell_core::types::ExitStatus;

pub type Pid = nix::unistd::Pid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Stopped,
    Done(ExitStatus),
}

#[derive(Debug)]
pub struct Job {
    pub id: usize,
    pub cmdline: String,
    pub pids: Vec<Pid>,
    pub status: JobStatus,
    pub foreground: bool,
}

/// The shell's job table.
#[derive(Debug, Default)]
pub struct JobTable {
    jobs: HashMap<usize, Job>,
    next_id: usize,
}

impl JobTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, cmdline: String, pids: Vec<Pid>, foreground: bool) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        self.jobs.insert(
            id,
            Job {
                id,
                cmdline,
                pids,
                status: JobStatus::Running,
                foreground,
            },
        );
        id
    }

    pub fn get(&self, id: usize) -> Option<&Job> {
        self.jobs.get(&id)
    }

    pub fn get_mut(&mut self, id: usize) -> Option<&mut Job> {
        self.jobs.get_mut(&id)
    }

    pub fn remove(&mut self, id: usize) -> Option<Job> {
        self.jobs.remove(&id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Job> {
        self.jobs.values()
    }
}
