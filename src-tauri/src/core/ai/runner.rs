//! In-memory AI runtime state and the persistent claim/process runner.
//!
//! The state deliberately holds no API key material: only the TTL preview
//! registry, the shutdown signal, and per-job cancellation flags live here.
//! Queue state itself is the SQLite tables; this struct is coordination only.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::core::skill_store::SkillStore;

use super::config::load_config;
use super::preview::{now_millis, PreviewEntry};
use super::repository::AiRepository;
use super::service;

#[derive(Default)]
pub struct AiRuntimeState {
    /// TTL preview registry; entries are removed atomically on enqueue.
    pub(crate) previews: Mutex<HashMap<String, PreviewEntry>>,
    shutdown: AtomicBool,
    cancel_flags: Mutex<HashMap<String, Arc<AtomicBool>>>,
    active_jobs: AtomicUsize,
}

impl AiRuntimeState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_cancelled(&self, job_id: &str) -> bool {
        self.cancel_flags
            .lock()
            .map(|flags| {
                flags
                    .get(job_id)
                    .map(|flag| flag.load(Ordering::SeqCst))
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    /// Register a running job and return its cancellation flag. The runner
    /// keeps the flag alive until the job task finishes.
    pub(crate) fn register_running(&self, job_id: &str) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        if let Ok(mut flags) = self.cancel_flags.lock() {
            flags.insert(job_id.to_string(), flag.clone());
        }
        self.active_jobs.fetch_add(1, Ordering::SeqCst);
        flag
    }

    pub(crate) fn unregister_running(&self, job_id: &str) {
        if let Ok(mut flags) = self.cancel_flags.lock() {
            flags.remove(job_id);
        }
        self.active_jobs.fetch_sub(1, Ordering::SeqCst);
    }

    /// Request cancellation of a running job (used by the phase-4 command;
    /// the runner and service check the flag before every network attempt).
    pub fn request_cancel(&self, job_id: &str) {
        if let Ok(flags) = self.cancel_flags.lock() {
            if let Some(flag) = flags.get(job_id) {
                flag.store(true, Ordering::SeqCst);
            }
        }
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }
}

/// Startup path: recover interrupted/cancelled state on a blocking thread,
/// then spawn the persistent runner.
pub fn start(store: Arc<SkillStore>, state: Arc<AiRuntimeState>) {
    tauri::async_runtime::spawn(async move {
        // Recovery must finish before the runner starts claiming jobs so the
        // frozen interrupted -> requeued / cancel propagation order is never
        // raced by a fresh claim.
        let recovery_store = store.clone();
        match tokio::task::spawn_blocking(move || {
            AiRepository::new(&recovery_store).recover_on_startup(now_millis())
        })
        .await
        {
            Ok(Ok(summary)) => {
                log::info!(
                    "AI analysis recovery: interrupted={} requeued={} cancelled_jobs={} cancelled_batches={}",
                    summary.interrupted,
                    summary.requeued,
                    summary.cancelled_jobs,
                    summary.cancelled_batches
                );
            }
            Ok(Err(error)) => {
                log::warn!("AI analysis startup recovery failed: {error:#}");
            }
            Err(error) => {
                log::warn!("AI analysis startup recovery task failed: {error}");
            }
        }
        runner_loop(store, state).await;
    });
}

async fn runner_loop(store: Arc<SkillStore>, state: Arc<AiRuntimeState>) {
    let active = Arc::new(AtomicUsize::new(0));
    loop {
        if state.shutdown.load(Ordering::SeqCst) {
            break;
        }
        let concurrency = current_concurrency(&store);
        if active.load(Ordering::SeqCst) >= concurrency {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }

        let now = now_millis();
        let claim_store = store.clone();
        let claimed = tokio::task::spawn_blocking(move || {
            AiRepository::new(&claim_store).claim_next_job(now)
        })
        .await;

        match claimed {
            Ok(Ok(Some(job))) => {
                active.fetch_add(1, Ordering::SeqCst);
                let flag = state.register_running(&job.job.id);
                let job_store = store.clone();
                let job_state = state.clone();
                let active_counter = active.clone();
                tokio::spawn(async move {
                    service::process_job(job_store, job_state, job).await;
                    drop(flag);
                    active_counter.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Ok(Ok(None)) => {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            Ok(Err(error)) => {
                log::warn!("AI analysis claim failed: {error:#}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(error) => {
                log::warn!("AI analysis claim task failed: {error}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

fn current_concurrency(store: &SkillStore) -> usize {
    match load_config(store) {
        Ok(config) => config.concurrency.clamp(1, 5) as usize,
        Err(_) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_flag_lifecycle_is_isolated_per_job() {
        let state = AiRuntimeState::new();
        let flag = state.register_running("job-a");
        assert!(!state.is_cancelled("job-a"));
        state.request_cancel("job-a");
        assert!(state.is_cancelled("job-a"));
        assert!(flag.load(Ordering::SeqCst));
        assert!(!state.is_cancelled("job-b"));
        state.unregister_running("job-a");
        assert!(!state.is_cancelled("job-a"));
    }
}
