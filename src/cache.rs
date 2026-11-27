//! Bounded, expiring values and cancellation-safe per-key request coalescing.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::{
    sync::{Mutex as AsyncMutex, OwnedMutexGuard},
    time::Instant,
};

pub struct Cache<T> {
    values: Mutex<HashMap<String, (Instant, usize, T)>>,
    flights: Mutex<HashMap<String, Weak<AsyncMutex<()>>>>,
    entries: usize,
    bytes: usize,
}

impl<T: Clone> Cache<T> {
    pub fn new(entries: usize, bytes: usize) -> Self {
        Self {
            values: Mutex::new(HashMap::new()),
            flights: Mutex::new(HashMap::new()),
            entries,
            bytes,
        }
    }

    pub fn get(&self, key: &str, max_age: Duration) -> Option<T> {
        if max_age.is_zero() {
            return None;
        }
        self.values
            .lock()
            .ok()?
            .get(key)
            .filter(|(stored, _, _)| stored.elapsed() < max_age)
            .map(|(_, _, value)| value.clone())
    }

    pub fn put(&self, key: String, value: T, size: usize) {
        if size > self.bytes {
            return;
        }
        let Ok(mut values) = self.values.lock() else {
            return;
        };
        values.remove(&key);
        values.retain(|_, (at, _, _)| at.elapsed() < Duration::from_secs(1800));
        let mut total: usize = values.values().map(|(_, n, _)| n).sum();
        while values.len() >= self.entries || total + size > self.bytes {
            let Some(oldest) = values
                .iter()
                .min_by_key(|(_, (at, _, _))| *at)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some((_, n, _)) = values.remove(&oldest) {
                total -= n;
            }
        }
        values.insert(key, (Instant::now(), size, value));
    }

    pub async fn flight(&self, key: &str, deadline: Instant) -> Result<OwnedMutexGuard<()>, ()> {
        let lock = {
            let mut flights = self.flights.lock().map_err(|_| ())?;
            flights.retain(|_, weak| weak.strong_count() > 0);
            if let Some(lock) = flights.get(key).and_then(Weak::upgrade) {
                lock
            } else {
                if flights.len() >= 1024 {
                    return Err(());
                }
                let lock = Arc::new(AsyncMutex::new(()));
                flights.insert(key.into(), Arc::downgrade(&lock));
                lock
            }
        };
        tokio::time::timeout_at(deadline, lock.lock_owned())
            .await
            .map_err(|_| ())
    }
}

#[cfg(test)]
mod tests;
