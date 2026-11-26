/*
 * Copyright (c) Mihir Paldhikar
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the “Software”), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED “AS IS”, WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 */

use hashbrown::HashMap;
use std::hash::Hash;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Notify;

const NUM_SHARDS: usize = 64;

static NEXT_SHARD: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static THREAD_SHARD_IDX: usize = NEXT_SHARD.fetch_add(1, Ordering::Relaxed) % NUM_SHARDS;
}

#[inline]
fn current_shard_idx() -> usize {
    THREAD_SHARD_IDX.with(|&idx| idx)
}

#[repr(align(128))]
struct Shard<Id, Info> {
    count: AtomicUsize,
    map: Option<Mutex<HashMap<Id, Info>>>,
}

pub struct ActiveSet<Id, Info> {
    inner: Arc<Inner<Id, Info>>,
}

struct Inner<Id, Info> {
    empty_notify: Notify,
    shards: Box<[Shard<Id, Info>]>,
}

pub struct ActiveGuard<Id, Info>
where
    Id: Eq + Hash,
{
    drop_guard: Arc<GuardDrop<Id, Info>>,
}

struct GuardDrop<Id, Info>
where
    Id: Eq + Hash,
{
    inner: Arc<Inner<Id, Info>>,
    shard_idx: usize,
    id: Id,
}

#[allow(dead_code)]
impl<Id, Info> ActiveSet<Id, Info>
where
    Id: Eq + Hash + Clone,
{
    pub fn new() -> Self {
        Self::internal_new(false)
    }

    pub fn new_with_telemetry(with_telemetry: bool) -> Self {
        Self::internal_new(with_telemetry)
    }

    fn internal_new(with_map: bool) -> Self {
        let shards = (0..NUM_SHARDS)
            .map(|_| Shard {
                count: AtomicUsize::new(0),
                map: if with_map {
                    Some(Mutex::new(HashMap::new()))
                } else {
                    None
                },
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Self {
            inner: Arc::new(Inner {
                empty_notify: Notify::new(),
                shards,
            }),
        }
    }

    pub fn insert(&self, id: Id, info: Info) -> ActiveGuard<Id, Info> {
        let shard_idx = current_shard_idx();
        let shard = &self.inner.shards[shard_idx];

        shard.count.fetch_add(1, Ordering::Relaxed);

        if let Some(map) = &shard.map {
            map.lock().unwrap().insert(id.clone(), info);
        }

        ActiveGuard {
            drop_guard: Arc::new(GuardDrop {
                inner: self.inner.clone(),
                shard_idx,
                id,
            }),
        }
    }

    pub fn len(&self) -> usize {
        self.inner
            .shards
            .iter()
            .map(|s| s.count.load(Ordering::Relaxed))
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn contains(&self, id: &Id) -> bool {
        for shard in &self.inner.shards {
            if shard.count.load(Ordering::Relaxed) == 0 {
                continue;
            }
            if let Some(map) = &shard.map {
                if map.lock().unwrap().contains_key(id) {
                    return true;
                }
            }
        }
        false
    }

    pub fn get(&self, id: &Id) -> Option<Info>
    where
        Info: Clone,
    {
        for shard in &self.inner.shards {
            if shard.count.load(Ordering::Relaxed) == 0 {
                continue;
            }
            if let Some(map) = &shard.map {
                if let Some(val) = map.lock().unwrap().get(id) {
                    return Some(val.clone());
                }
            }
        }
        None
    }

    pub fn snapshot(&self) -> HashMap<Id, Info>
    where
        Id: Clone,
        Info: Clone,
    {
        let mut out = HashMap::new();
        for shard in &self.inner.shards {
            if shard.count.load(Ordering::Relaxed) == 0 {
                continue;
            }
            if let Some(map) = &shard.map {
                let guard = map.lock().unwrap();
                for (k, v) in guard.iter() {
                    out.insert(k.clone(), v.clone());
                }
            }
        }
        out
    }

    pub async fn wait_for_zero(&self) {
        loop {
            let notified = self.inner.empty_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if self
                .inner
                .shards
                .iter()
                .all(|s| s.count.load(Ordering::Acquire) == 0)
            {
                return;
            }

            notified.await;
        }
    }
}

#[allow(dead_code)]
impl<Id, Info> ActiveGuard<Id, Info>
where
    Id: Eq + Hash,
    Info: Clone,
{
    pub fn id(&self) -> &Id {
        &self.drop_guard.id
    }

    pub fn update(&self, f: impl FnOnce(&mut Info)) {
        let shard = &self.drop_guard.inner.shards[self.drop_guard.shard_idx];
        if let Some(map) = &shard.map {
            if let Some(info) = map.lock().unwrap().get_mut(&self.drop_guard.id) {
                f(info);
            }
        }
    }

    pub fn value(&self) -> Option<Info> {
        let shard = &self.drop_guard.inner.shards[self.drop_guard.shard_idx];
        shard
            .map
            .as_ref()?
            .lock()
            .unwrap()
            .get(&self.drop_guard.id)
            .cloned()
    }
}

impl<Id, Info> Clone for ActiveGuard<Id, Info>
where
    Id: Eq + Hash,
{
    fn clone(&self) -> Self {
        Self {
            drop_guard: self.drop_guard.clone(),
        }
    }
}

impl<Id, Info> Drop for GuardDrop<Id, Info>
where
    Id: Eq + Hash,
{
    fn drop(&mut self) {
        let shard = &self.inner.shards[self.shard_idx];
        if let Some(map) = &shard.map {
            map.lock().unwrap().remove(&self.id);
        }
        shard.count.fetch_sub(1, Ordering::Relaxed);

        let prev = shard.count.fetch_sub(1, Ordering::Release);
        if prev == 1 {
            self.inner.empty_notify.notify_waiters();
        }
    }
}

unsafe impl<Id, Info> Send for ActiveSet<Id, Info>
where
    Id: Send + Eq + Hash + Clone,
    Info: Send,
{
}

unsafe impl<Id, Info> Sync for ActiveSet<Id, Info>
where
    Id: Send + Sync + Eq + Hash + Clone,
    Info: Send + Sync,
{
}
