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
use std::hash::Hash;
use std::num::NonZeroU32;
use std::ptr::NonNull;

const DEFAULT_SHARD_COUNT: usize = 16;
const MIN_BUCKET_BYTES: usize = 4 * 1024;
const MAX_BUCKET_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct Symbol(NonZeroU32);

impl Symbol {
    #[inline]
    fn from_index(index: usize) -> Self {
        debug_assert!(index < u32::MAX as usize);

        Symbol(unsafe { NonZeroU32::new_unchecked((index as u32) + 1) })
    }

    #[inline]
    fn index(self) -> usize {
        (self.0.get() - 1) as usize
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

struct Shard {
    entries: RwLock<HashTable<(u64, &'static str, Symbol)>>,
}

pub struct SymbolInterner {
    shards: Box<[Shard]>,
    storage: RwLock<Storage>,
    hash_builder: ahash::RandomState,
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
        let count = count.clamp(1, 1 << 16).next_power_of_two();
        let per_shard = symbols.div_ceil(count);

        let shards = (0..count)
            .map(|_| Shard {
                entries: RwLock::new(HashTable::with_capacity(per_shard)),
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Self {
            shards,
            storage: RwLock::new(Storage {
                buckets: Vec::new(),
                strings: Vec::with_capacity(symbols),
            }),
            hash_builder: ahash::RandomState::new(),
            shard_mask: (count - 1) as u64,
        }
    }

    pub fn get_or_intern(&self, value: &str) -> Symbol {
        let hash = self.hash_builder.hash_one(value);
        let shard = self.shard_for_hash(hash);

        {
            let entries = shard.entries.read();
            if let Some(&(_, _, symbol)) = entries.find(hash, |&(_, text, _)| text == value) {
                return symbol;
            }
        }

        let mut entries = shard.entries.write();

        match entries.entry(
            hash,
            |&(_, text, _)| text == value,
            |&(cached_hash, _, _)| cached_hash,
        ) {
            Entry::Occupied(entry) => entry.get().2,
            Entry::Vacant(entry) => {
                let symbol = {
                    let mut storage = self.storage.write();
                    let index = storage.strings.len();
                    assert!(index < u32::MAX as usize, "SymbolInterner overflow");
                    let text = storage.alloc(value);
                    storage.strings.push(text);
                    (text, Symbol::from_index(index))
                };
                entry.insert((hash, symbol.0, symbol.1));
                symbol.1
            }
        }
    }

    pub fn get(&self, value: &str) -> Option<Symbol> {
        let hash = self.hash_builder.hash_one(value);
        let shard = self.shard_for_hash(hash);

        let entries = shard.entries.read();

        entries
            .find(hash, |&(_, text, _)| text == value)
            .map(|&(_, _, symbol)| symbol)
    }

    pub fn resolve(&self, symbol: Symbol) -> Option<&str> {
        let storage = self.storage.read();
        storage.strings.get(symbol.index()).copied()
    }

    pub fn contains(&self, value: &str) -> bool {
        self.get(value).is_some()
    }

    pub fn len(&self) -> usize {
        self.storage.read().strings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.storage.read().strings.is_empty()
    }

    #[inline]
    fn shard_for_hash(&self, hash: u64) -> &Shard {
        let index = (hash & self.shard_mask) as usize;
        &self.shards[index]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_interner() {
        let interner = SymbolInterner::new();
        interner.get_or_intern("hello world");
        assert_eq!(interner.len(), 1);
        assert_eq!(interner.is_empty(), false);
    }
}
