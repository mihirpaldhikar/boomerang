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
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::ptr;
use std::task::{Context, Poll};

const INLINE_CAP: usize = 128;

#[repr(C, align(16))]
struct InlineBuf([MaybeUninit<u8>; INLINE_CAP]);

pub struct SmallFuture<'a, T> {
    inline: InlineBuf,
    heap_ptr: *mut u8,
    poll_fn: unsafe fn(*mut u8, &mut Context<'_>) -> Poll<T>,
    drop_fn: unsafe fn(*mut u8),
    _marker: PhantomData<&'a mut ()>,
}

// SAFETY: `SmallFuture::new` requires `F: Send`.
unsafe impl<'a, T> Send for SmallFuture<'a, T> {}

impl<'a, T> SmallFuture<'a, T> {
    #[inline]
    pub fn new<F>(fut: F) -> Self
    where
        F: Future<Output = T> + Send + 'a,
    {
        unsafe fn poll_impl<F: Future>(ptr: *mut u8, cx: &mut Context<'_>) -> Poll<F::Output> {
            unsafe { Pin::new_unchecked(&mut *(ptr as *mut F)).poll(cx) }
        }

        unsafe fn drop_inline<F>(ptr: *mut u8) {
            unsafe { ptr::drop_in_place(ptr as *mut F) };
        }

        unsafe fn drop_heap<F>(ptr: *mut u8) {
            unsafe { drop(Box::from_raw(ptr as *mut F)) };
        }

        if size_of::<F>() <= INLINE_CAP && align_of::<F>() <= 16 {
            let mut this = Self {
                inline: InlineBuf([MaybeUninit::uninit(); INLINE_CAP]),
                heap_ptr: ptr::null_mut(),
                poll_fn: poll_impl::<F>,
                drop_fn: drop_inline::<F>,
                _marker: PhantomData,
            };
            unsafe { ptr::write(this.inline.0.as_mut_ptr() as *mut F, fut) };
            this
        } else {
            let heap_ptr = Box::into_raw(Box::new(fut)) as *mut u8;
            Self {
                inline: InlineBuf([MaybeUninit::uninit(); INLINE_CAP]),
                heap_ptr,
                poll_fn: poll_impl::<F>,
                drop_fn: drop_heap::<F>,
                _marker: PhantomData,
            }
        }
    }
}

impl<'a, T> Future for SmallFuture<'a, T> {
    type Output = T;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        let ptr = if this.heap_ptr.is_null() {
            this.inline.0.as_mut_ptr() as *mut u8
        } else {
            this.heap_ptr
        };
        unsafe { (this.poll_fn)(ptr, cx) }
    }
}

impl<'a, T> Drop for SmallFuture<'a, T> {
    #[inline]
    fn drop(&mut self) {
        let ptr = if self.heap_ptr.is_null() {
            self.inline.0.as_mut_ptr() as *mut u8
        } else {
            self.heap_ptr
        };
        unsafe { (self.drop_fn)(ptr) };
    }
}
