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
use hashbrown::HashTable;
use hashbrown::hash_table::Entry;
use parking_lot::RwLock;
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::Arc;

const DEFAULT_SHARD_COUNT: usize = 16;

pub struct StringInterner {
    shards: Box<[Shard]>,
    hash_builder: ahash::RandomState,
    shard_shift: u32,
}

struct Shard {
    entries: RwLock<HashTable<Arc<str>>>,
}

impl StringInterner {
    pub fn new() -> Self {
        Self::with_shard_count(DEFAULT_SHARD_COUNT)
    }

    pub fn with_shard_count(count: usize) -> Self {
        Self::with_capacity_and_shards(0, count)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_capacity_and_shards(capacity, DEFAULT_SHARD_COUNT)
    }

    pub fn with_capacity_and_shards(capacity: usize, count: usize) -> Self {
        let count = count.clamp(1, 1 << 16).next_power_of_two();
        let per_shard = capacity.div_ceil(count);

        let shards = (0..count)
            .map(|_| Shard {
                entries: RwLock::new(HashTable::with_capacity(per_shard)),
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Self {
            shards,
            hash_builder: ahash::RandomState::new(),
            shard_shift: 64 - count.trailing_zeros(),
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
            let entries = shard.entries.read();

            if let Some(existing) = entries.find(hash, |k| &**k == value) {
                return Arc::clone(existing);
            }
        }

        let mut entries = shard.entries.write();

        match entries.entry(
            hash,
            |k| &**k == value,
            |k| self.hash_builder.hash_one(&**k),
        ) {
            Entry::Occupied(entry) => Arc::clone(entry.get()),
            Entry::Vacant(entry) => {
                let interned: Arc<str> = Arc::from(value);
                entry.insert(Arc::clone(&interned));
                interned
            }
        }
    }

    pub fn get(&self, value: &str) -> Option<Arc<str>> {
        let hash = self.hash(value);
        let shard = self.shard_for_hash(hash);

        let entries = shard.entries.read();
        entries.find(hash, |k| &**k == value).map(Arc::clone)
    }

    pub fn contains(&self, value: &str) -> bool {
        self.get(value).is_some()
    }

    pub fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| shard.entries.read().len())
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.shards
            .iter()
            .all(|shard| shard.entries.read().is_empty())
    }

    pub fn reserve(&self, additional: usize) {
        let per_shard = additional.div_ceil(self.shards.len());

        for shard in self.shards.iter() {
            shard
                .entries
                .write()
                .reserve(per_shard, |k| self.hash_builder.hash_one(&**k));
        }
    }

    pub fn gc(&self) -> usize {
        let mut removed = 0;

        for shard in self.shards.iter() {
            let mut entries = shard.entries.write();

            let before = entries.len();
            entries.retain(|k| Arc::strong_count(k) > 1);
            removed += before - entries.len();

            if entries.len() < before / 2 {
                entries.shrink_to_fit(|k| self.hash_builder.hash_one(&**k));
            }
        }

        removed
    }

    pub fn clear(&self) {
        for shard in self.shards.iter() {
            shard.entries.write().clear();
        }
    }

    fn hash(&self, value: &str) -> u64 {
        let mut hasher = self.hash_builder.build_hasher();
        value.hash(&mut hasher);
        hasher.finish()
    }

    fn shard_for_hash(&self, hash: u64) -> &Shard {
        let index = (hash >> self.shard_shift) as usize;
        &self.shards[index]
    }
}

impl Default for StringInterner {
    fn default() -> Self {
        Self::new()
    }
}
