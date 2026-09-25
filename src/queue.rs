use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use anyhow::{Result, ensure};
use tokio::{sync::Mutex, time::Instant};

use crate::discord::Message;

const MAX_JOBS: usize = 64;
const MAX_HISTORY: usize = 4096;
const HISTORY_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);
type ReleaseKey = (String, i64, String);

#[derive(Clone, Default)]
pub struct Queue(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    pending: Vec<(Instant, Job)>,
    active: HashSet<ReleaseKey>,
    deliveries: HashMap<String, Instant>,
    completed: HashMap<ReleaseKey, Instant>,
    cooldown: Option<Instant>,
    next_id: u64,
}

pub struct EnqueueResult {
    pub delivery_inserted: bool,
    pub jobs_inserted: usize,
}

pub struct Job {
    pub id: u64,
    pub delivery: String,
    pub repository: String,
    pub destination: String,
    pub routes: Vec<String>,
    pub attempts: i64,
    pub message: Arc<Message>,
    release_id: i64,
}

impl Job {
    fn key(&self) -> ReleaseKey {
        (
            self.repository.clone(),
            self.release_id,
            self.destination.clone(),
        )
    }
}

fn remember<K: Eq + std::hash::Hash + Clone>(history: &mut HashMap<K, Instant>, key: K) {
    if history.len() >= MAX_HISTORY
        && let Some(oldest) = history
            .iter()
            .min_by_key(|(_, time)| *time)
            .map(|(key, _)| key.clone())
    {
        history.remove(&oldest);
    }
    history.insert(key, Instant::now());
}

impl Queue {
    pub async fn enqueue(
        &self,
        delivery: &str,
        repository: &str,
        release_id: i64,
        destinations: &BTreeMap<String, Vec<String>>,
        message: &Message,
    ) -> Result<EnqueueResult> {
        let mut state = self.0.lock().await;
        let now = Instant::now();
        state
            .deliveries
            .retain(|_, time| now.duration_since(*time) < HISTORY_TTL);
        state
            .completed
            .retain(|_, time| now.duration_since(*time) < HISTORY_TTL);

        if state.deliveries.contains_key(delivery) {
            return Ok(EnqueueResult {
                delivery_inserted: false,
                jobs_inserted: 0,
            });
        }

        let destinations: Vec<_> = destinations
            .iter()
            .filter(|(destination, _)| {
                let key = (repository.to_owned(), release_id, (*destination).clone());
                !state.active.contains(&key) && !state.completed.contains_key(&key)
            })
            .collect();
        ensure!(
            state.active.len() + destinations.len() <= MAX_JOBS,
            "delivery queue is full"
        );

        let jobs_inserted = destinations.len();
        let message = Arc::new(message.clone());
        for (destination, routes) in destinations {
            state.next_id += 1;
            let job = Job {
                id: state.next_id,
                delivery: delivery.to_owned(),
                repository: repository.to_owned(),
                release_id,
                destination: destination.clone(),
                routes: routes.clone(),
                attempts: 0,
                message: message.clone(),
            };
            state.active.insert(job.key());
            state.pending.push((now, job));
        }
        remember(&mut state.deliveries, delivery.to_owned());

        Ok(EnqueueResult {
            delivery_inserted: true,
            jobs_inserted,
        })
    }

    pub async fn claim(&self) -> Option<Job> {
        let mut state = self.0.lock().await;
        let now = Instant::now();
        if state.cooldown.is_some_and(|until| until > now) {
            return None;
        }
        let index = state
            .pending
            .iter()
            .enumerate()
            .filter(|(_, (due, _))| *due <= now)
            .min_by_key(|(_, (due, job))| (*due, job.id))
            .map(|(index, _)| index)?;
        let (_, mut job) = state.pending.swap_remove(index);
        job.attempts += 1;
        Some(job)
    }

    pub async fn finish(&self, job: Job, retry: bool, delay: i64, sent: bool, rate_limited: bool) {
        let mut state = self.0.lock().await;
        let due = Instant::now() + Duration::from_secs(delay.max(0).cast_unsigned());
        if rate_limited {
            state.cooldown = Some(state.cooldown.map_or(due, |until| until.max(due)));
        }
        if retry {
            state.pending.push((due, job));
        } else {
            state.active.remove(&job.key());
            if sent {
                remember(&mut state.completed, job.key());
            } else {
                // Permit GitHub redelivery after a permanent or exhausted failure.
                state.deliveries.remove(&job.delivery);
            }
        }
    }
}
