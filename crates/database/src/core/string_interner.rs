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
use ahash::AHashMap;
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::{Arc, RwLock};

const DEFAULT_SHARD_COUNT: usize = 64;

pub struct StringInterner {
    shards: Box<[Shard]>,
    hash_builder: ahash::RandomState,
}

struct Shard {
    entries: RwLock<AHashMap<Arc<str>, ()>>,
}

impl StringInterner {
    pub fn new() -> Self {
        Self::with_shard_count(DEFAULT_SHARD_COUNT)
    }

    pub fn with_shard_count(count: usize) -> Self {
        let count = count.max(1).next_power_of_two();

        let shards = (0..count)
            .map(|_| Shard {
                entries: RwLock::new(AHashMap::new()),
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Self {
            shards,
            hash_builder: ahash::RandomState::new(),
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_capacity_and_shards(capacity, DEFAULT_SHARD_COUNT)
    }

    pub fn with_capacity_and_shards(capacity: usize, count: usize) -> Self {
        let count = count.max(1).next_power_of_two();
        let per_shard = capacity.div_ceil(count);

        let shards = (0..count)
            .map(|_| Shard {
                entries: RwLock::new(AHashMap::with_capacity(per_shard)),
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Self {
            shards,
            hash_builder: ahash::RandomState::new(),
        }
    }

    pub fn intern<S>(&self, value: S) -> Arc<str>
    where
        S: AsRef<str>,
    {
        let value = value.as_ref();
        let hash = self.hash(value);
        let shard = self.shard_for_hash(hash);

        {
            let entries = shard.entries.read().expect("read lock poisoned");

            if let Some((existing, _)) = entries.get_key_value(value) {
                return Arc::clone(existing);
            }
        }

        let mut entries = shard.entries.write().expect("write lock poisoned");

        if let Some((existing, _)) = entries.get_key_value(value) {
            return Arc::clone(existing);
        }

        let interned: Arc<str> = Arc::from(value);
        entries.insert(Arc::clone(&interned), ());
        interned
    }

    pub fn intern_string(&self, value: String) -> Arc<str> {
        let hash = self.hash(&value);
        let shard = self.shard_for_hash(hash);

        {
            let entries = shard.entries.read().expect("read lock poisoned");

            if let Some((existing, _)) = entries.get_key_value(value.as_str()) {
                return Arc::clone(existing);
            }
        }

        let mut entries = shard.entries.write().expect("write lock poisoned");

        if let Some((existing, _)) = entries.get_key_value(value.as_str()) {
            return Arc::clone(existing);
        }

        let interned: Arc<str> = Arc::from(value.into_boxed_str());
        entries.insert(Arc::clone(&interned), ());
        interned
    }

    pub fn get(&self, value: &str) -> Option<Arc<str>> {
        let hash = self.hash(value);
        let shard = self.shard_for_hash(hash);

        let entries = shard.entries.read().expect("read lock poisoned");
        entries
            .get_key_value(value)
            .map(|(existing, _)| Arc::clone(existing))
    }

    pub fn contains(&self, value: &str) -> bool {
        self.get(value).is_some()
    }

    pub fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| shard.entries.read().expect("read lock poisoned").len())
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.shards
            .iter()
            .all(|shard| shard.entries.read().expect("read lock poisoned").is_empty())
    }

    pub fn reserve(&self, additional: usize) {
        let per_shard = additional.div_ceil(self.shards.len());

        for shard in self.shards.iter() {
            shard
                .entries
                .write()
                .expect("write lock poisoned")
                .reserve(per_shard);
        }
    }

    pub fn clear(&self) {
        for shard in self.shards.iter() {
            shard.entries.write().expect("write lock poisoned").clear();
        }
    }

    fn hash(&self, value: &str) -> u64 {
        let mut hasher = self.hash_builder.build_hasher();
        value.hash(&mut hasher);
        hasher.finish()
    }

    fn shard_for_hash(&self, hash: u64) -> &Shard {
        let index = hash as usize & (self.shards.len() - 1);
        &self.shards[index]
    }
}

impl Default for StringInterner {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for StringInterner {
    fn drop(&mut self) {
        self.clear();
    }
}
