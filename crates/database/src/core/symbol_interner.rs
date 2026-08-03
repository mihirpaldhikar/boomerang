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
use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use std::cell::RefCell;
use std::hash::{BuildHasher, Hash};
use std::num::NonZeroU32;
use std::ptr::NonNull;

const DEFAULT_SHARD_COUNT: usize = 16;
const MIN_BUCKET_BYTES: usize = 4 * 1024;
const MAX_BUCKET_BYTES: usize = 256 * 1024;
const CACHE_CAPACITY: usize = 1024; // MUST be a power of 2
const _: () = assert!(CACHE_CAPACITY.is_power_of_two());
const CACHE_MASK: u64 = (CACHE_CAPACITY - 1) as u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct Symbol(NonZeroU32);

impl Symbol {
    const INDEX_MASK: u32 = 0x00FF_FFFF; // Bottom 24 bits.

    fn new(shard_id: u8, index: usize) -> Self {
        assert!(index < Self::INDEX_MASK as usize, "Shard index overflow");

        let raw = ((shard_id as u32) << 24) | ((index as u32) + 1);
        Symbol(unsafe { NonZeroU32::new_unchecked(raw) })
    }

    #[inline]
    fn shard_id(&self) -> usize {
        (self.0.get() >> 24) as usize
    }

    #[inline]
    fn index(&self) -> usize {
        (self.0.get() & Self::INDEX_MASK) as usize - 1
    }

    #[inline]
    pub fn into_raw(self) -> u32 {
        self.0.get()
    }

    #[inline]
    pub fn from_raw(raw: u32) -> Option<Self> {
        NonZeroU32::new(raw).map(Symbol)
    }
}

struct Bucket {
    ptr: NonNull<u8>,
    layout: Layout,
    cursor: usize,
}

// Safety: Bucket owns its raw memory buffer, so it can be safely shared between threads.
unsafe impl Send for Bucket {}
unsafe impl Sync for Bucket {}

impl Bucket {
    fn new(capacity: usize) -> Self {
        let layout = Layout::array::<u8>(capacity).expect("valid layout");
        assert!(capacity > 0, "capacity must be greater than 0");

        let ptr = unsafe { alloc(layout) };
        let ptr = NonNull::new(ptr).unwrap_or_else(|| handle_alloc_error(layout));

        Self {
            ptr,
            layout,
            cursor: 0,
        }
    }

    #[inline]
    fn capacity(&self) -> usize {
        self.layout.size()
    }

    fn alloc(&mut self, value: &str) -> Option<&'static str> {
        let len = value.len();

        if self.capacity() - self.cursor < len {
            return None;
        }

        unsafe {
            let destination = self.ptr.as_ptr().add(self.cursor);
            std::ptr::copy_nonoverlapping(value.as_ptr(), destination, len);
            self.cursor += len;

            let slice = std::slice::from_raw_parts(destination, len);
            Some(std::str::from_utf8_unchecked(slice))
        }
    }
}

impl Drop for Bucket {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

struct Storage {
    buckets: Vec<Bucket>,
    strings: Vec<&'static str>,
}

impl Storage {
    fn alloc(&mut self, value: &str) -> &'static str {
        let len = value.len();

        if len == 0 {
            return "";
        }

        if let Some(bucket) = self.buckets.last_mut() {
            if let Some(text) = bucket.alloc(value) {
                return text;
            }
        }

        let next_cap = self
            .buckets
            .last()
            .map_or(MIN_BUCKET_BYTES, |bucket| {
                (bucket.capacity() * 2).min(MAX_BUCKET_BYTES)
            })
            .max(len);

        let mut new_bucket = Bucket::new(next_cap);
        let text = new_bucket
            .alloc(value)
            .expect("new bucket has enough capacity");

        self.buckets.push(new_bucket);

        text
    }
}

struct ShardInner {
    entries: HashTable<(u64, Symbol)>,
    storage: Storage,
}

struct Shard {
    inner: RwLock<ShardInner>,
}

struct ThreadLocalCache {
    entries: [Option<(u64, &'static str, Symbol)>; CACHE_CAPACITY],
}

impl ThreadLocalCache {
    fn new() -> Self {
        Self {
            entries: [None; CACHE_CAPACITY],
        }
    }
}

thread_local! {
    static LOCAL_CACHE: RefCell<ThreadLocalCache> = RefCell::new(ThreadLocalCache::new());
}

pub struct SymbolInterner {
    shards: Box<[Shard]>,
    hash_builder: rustc_hash::FxBuildHasher,
    shard_mask: u64,
}

impl Default for SymbolInterner {
    fn default() -> Self {
        Self::new()
    }
}

impl SymbolInterner {
    pub fn new() -> Self {
        Self::with_capacity_and_shards(0, DEFAULT_SHARD_COUNT)
    }

    pub fn with_capacity(symbols: usize) -> Self {
        Self::with_capacity_and_shards(symbols, DEFAULT_SHARD_COUNT)
    }

    pub fn with_capacity_and_shards(symbols: usize, count: usize) -> Self {
        let count = count.clamp(1, 256).next_power_of_two();
        let per_shard = symbols.div_ceil(count);

        let shards = (0..count)
            .map(|_| Shard {
                inner: RwLock::new(ShardInner {
                    entries: HashTable::with_capacity(per_shard),
                    storage: Storage {
                        buckets: Vec::new(),
                        strings: Vec::with_capacity(symbols),
                    },
                }),
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Self {
            shards,
            hash_builder: rustc_hash::FxBuildHasher::default(),
            shard_mask: (count - 1) as u64,
        }
    }

    pub fn get_or_intern(&self, value: &str) -> Symbol {
        let hash = self.hash_builder.hash_one(value);
        let cache_index = (hash & CACHE_MASK) as usize;

        let mut hit = None;
        LOCAL_CACHE.with(|cache| {
            let cache = cache.borrow();

            if let Some((cached_hash, cached_value, symbol)) = cache.entries[cache_index] {
                if cached_hash == hash && cached_value == value {
                    hit = Some(symbol);
                }
            }
        });

        if let Some(symbol) = hit {
            return symbol;
        }

        let shard_id = (hash & self.shard_mask) as usize;
        let shard = &self.shards[shard_id];

        let mut resolved = None;

        {
            let inner = shard.inner.read();
            if let Some(&(_, symbol)) = inner.entries.find(hash, |&(_, symbol)| {
                inner.storage.strings[symbol.index()] == value
            }) {
                resolved = Some((inner.storage.strings[symbol.index()], symbol));
            }
        }

        let (text, symbol) = if let Some(res) = resolved {
            res
        } else {
            let mut inner = shard.inner.write();

            let ShardInner { storage, entries } = &mut *inner;

            match entries.entry(
                hash,
                |&(_, symbol)| storage.strings[symbol.index()] == value,
                |&(cached_hash, _)| cached_hash,
            ) {
                Entry::Occupied(entry) => {
                    let symbol = entry.get().1;
                    (storage.strings[symbol.index()], symbol)
                }
                Entry::Vacant(entry) => {
                    let index = storage.strings.len();
                    let text = storage.alloc(value);
                    storage.strings.push(text);

                    let symbol = Symbol::new(shard_id as u8, index);
                    entry.insert((hash, symbol));
                    (text, symbol)
                }
            }
        };

        LOCAL_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            cache.entries[cache_index] = Some((hash, text, symbol));
        });

        symbol
    }

    pub fn get(&self, value: &str) -> Option<Symbol> {
        let hash = self.hash_builder.hash_one(value);
        let cache_index = (hash & CACHE_MASK) as usize;

        let mut hit = None;
        LOCAL_CACHE.with(|cache| {
            let cache = cache.borrow();

            if let Some((cached_hash, cached_value, symbol)) = cache.entries[cache_index] {
                if cached_hash == hash && cached_value == value {
                    hit = Some(symbol);
                }
            }
        });

        if let Some(symbol) = hit {
            return Some(symbol);
        }

        let shard_id = (hash & self.shard_mask) as usize;
        let shard = &self.shards[shard_id];

        let inner = shard.inner.read();

        let symbol = inner
            .entries
            .find(hash, |&(_, symbol)| {
                inner.storage.strings[symbol.index()] == value
            })
            .map(|&(_, symbol)| symbol)?;

        let text = inner.storage.strings[symbol.index()];

        LOCAL_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            cache.entries[cache_index] = Some((hash, text, symbol));
        });

        Some(symbol)
    }

    pub fn resolve(&self, symbol: Symbol) -> Option<&str> {
        let shard_id = symbol.shard_id();
        let index = symbol.index();

        let shard = self.shards.get(shard_id)?;
        let inner = shard.inner.read();
        inner.storage.strings.get(index).copied()
    }

    pub fn contains(&self, value: &str) -> bool {
        self.get(value).is_some()
    }

    pub fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| shard.inner.read().storage.strings.len())
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_interner() {
        let interner = SymbolInterner::with_capacity(10);

        interner.get_or_intern("hello");
        interner.get_or_intern("hello");

        assert_eq!(interner.len(), 1)
    }
}
