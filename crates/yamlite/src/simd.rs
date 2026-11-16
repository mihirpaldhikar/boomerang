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

#[derive(Clone, Copy, Debug)]
pub struct ChunkMasks {
    /// Bit `i` is 1 iff byte `i` is `\n` or `\r`.
    pub newlines: u32,
    /// Bit `i` is 1 iff byte `i` is a YAML structural character.
    pub structural: u32,
    /// Bit `i` is 1 iff byte `i` is ASCII space (`0x20`).
    pub spaces: u32,
    /// True iff all bytes in the chunk are 7-bit ASCII (`< 0x80`).
    pub all_ascii: bool,
}

#[inline]
pub fn count_spaces(bytes: &[u8], pos: usize) -> usize {
    #[cfg(all(target_arch = "x86_64", feature = "std"))]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: AVX2 support verified at runtime.
            return unsafe { count_spaces_avx2(bytes, pos) };
        }
        return count_spaces_swar(bytes, pos);
    }
    #[cfg(all(target_arch = "x86_64", not(feature = "std"), target_feature = "avx2"))]
    {
        // SAFETY: AVX2 enabled at compile time.
        return unsafe { count_spaces_avx2(bytes, pos) };
    }
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: NEON is baseline on aarch64.
        return unsafe { count_spaces_neon(bytes, pos) };
    }
    #[allow(unreachable_code)]
    count_spaces_swar(bytes, pos)
}

#[inline]
pub fn scan_chunk(bytes: &[u8], pos: usize) -> (ChunkMasks, usize) {
    let rem = bytes.len().saturating_sub(pos);

    #[cfg(all(target_arch = "x86_64", feature = "std"))]
    {
        if rem >= 32 && std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: `rem >= 32` guarantees 32 readable bytes, AVX2 verified.
            return (unsafe { scan_32_avx2(bytes.as_ptr().add(pos)) }, 32);
        }
        return scan_tail_scalar(&bytes[pos..]);
    }
    #[cfg(all(target_arch = "x86_64", not(feature = "std"), target_feature = "avx2"))]
    {
        if rem >= 32 {
            // SAFETY: `rem >= 32` and AVX2 enabled at compile time.
            return (unsafe { scan_32_avx2(bytes.as_ptr().add(pos)) }, 32);
        }
        return scan_tail_scalar(&bytes[pos..]);
    }
    #[cfg(target_arch = "aarch64")]
    {
        if rem >= 16 {
            // SAFETY: `rem >= 16` and NEON is baseline on aarch64.
            return (unsafe { scan_16_neon(bytes.as_ptr().add(pos)) }, 16);
        }
        return scan_tail_scalar(&bytes[pos..]);
    }
    #[allow(unreachable_code)]
    scan_tail_scalar(&bytes[pos..])
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn count_spaces_avx2(bytes: &[u8], mut pos: usize) -> usize {
    use core::arch::x86_64::*;
    let start = pos;
    let sp = _mm256_set1_epi8(b' ' as i8);
    while pos + 32 <= bytes.len() {
        let v = _mm256_loadu_si256(bytes.as_ptr().add(pos) as *const __m256i);
        let mask = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, sp)) as u32;
        if mask != u32::MAX {
            return (pos - start) + (!mask).trailing_zeros() as usize;
        }
        pos += 32;
    }
    (pos - start) + count_spaces_swar(bytes, pos)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn scan_32_avx2(ptr: *const u8) -> ChunkMasks {
    use core::arch::x86_64::*;

    let v = _mm256_loadu_si256(ptr as *const __m256i);
    let all_ascii = _mm256_movemask_epi8(v) == 0;

    let spaces = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(b' ' as i8))) as u32;
    let lf = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'\n' as i8))) as u32;
    let cr = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'\r' as i8))) as u32;

    // Exact 0-false-positive dual-nibble classification:
    // bit 0 (1): hi=0x2 (! " # % & ' * , -)
    // bit 1 (2): hi=0x3 (: > ?)
    // bit 2 (4): hi=0x5 ([ \ ])
    // bit 3 (8): hi=0x7 ({ | })
    let lo_nib = _mm256_and_si256(v, _mm256_set1_epi8(0x0f));
    let hi_nib = _mm256_and_si256(_mm256_srli_epi16(v, 4), _mm256_set1_epi8(0x0f));

    let lo_lut = _mm256_setr_epi8(
        0, 1, 1, 1, 0, 1, 1, 1, 0, 0, 3, 12, 13, 13, 2, 2, 0, 1, 1, 1, 0, 1, 1, 1, 0, 0, 3, 12, 13,
        13, 2, 2,
    );
    let hi_lut = _mm256_setr_epi8(
        0, 0, 1, 2, 0, 4, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 0, 4, 0, 8, 0, 0, 0, 0, 0, 0,
        0, 0,
    );

    let hit = _mm256_and_si256(
        _mm256_shuffle_epi8(lo_lut, lo_nib),
        _mm256_shuffle_epi8(hi_lut, hi_nib),
    );
    let is_zero = _mm256_cmpeq_epi8(hit, _mm256_setzero_si256());
    let structural = !(_mm256_movemask_epi8(is_zero) as u32);

    ChunkMasks {
        newlines: lf | cr,
        structural,
        spaces,
        all_ascii,
    }
}

#[cfg(target_arch = "aarch64")]
unsafe fn neon_movemask(cmp: core::arch::aarch64::uint8x16_t) -> u16 {
    use core::arch::aarch64::*;
    unsafe {
        const WEIGHTS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
        let w = vld1q_u8(WEIGHTS.as_ptr());
        let masked = vandq_u8(cmp, w);
        let lo = vaddv_u8(vget_low_u8(masked)) as u16;
        let hi = vaddv_u8(vget_high_u8(masked)) as u16;
        lo | (hi << 8)
    }
}

#[cfg(target_arch = "aarch64")]
unsafe fn count_spaces_neon(bytes: &[u8], mut pos: usize) -> usize {
    use core::arch::aarch64::*;
    unsafe {
        let start = pos;
        let sp = vdupq_n_u8(b' ');
        while pos + 16 <= bytes.len() {
            let v = vld1q_u8(bytes.as_ptr().add(pos));
            let mask = neon_movemask(vceqq_u8(v, sp));
            if mask != u16::MAX {
                return (pos - start) + (!mask).trailing_zeros() as usize;
            }
            pos += 16;
        }
        (pos - start) + count_spaces_swar(bytes, pos)
    }
}

#[cfg(target_arch = "aarch64")]
unsafe fn scan_16_neon(ptr: *const u8) -> ChunkMasks {
    use core::arch::aarch64::*;
    const LO_LUT: [u8; 16] = [0, 1, 1, 1, 0, 1, 1, 1, 0, 0, 3, 12, 13, 13, 2, 2];
    const HI_LUT: [u8; 16] = [0, 0, 1, 2, 0, 4, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0];

    unsafe {
        let v = vld1q_u8(ptr);
        let all_ascii = vmaxvq_u8(v) < 0x80;
        let spaces = neon_movemask(vceqq_u8(v, vdupq_n_u8(b' '))) as u32;
        let lf = neon_movemask(vceqq_u8(v, vdupq_n_u8(b'\n'))) as u32;
        let cr = neon_movemask(vceqq_u8(v, vdupq_n_u8(b'\r'))) as u32;

        let lo = vqtbl1q_u8(vld1q_u8(LO_LUT.as_ptr()), vandq_u8(v, vdupq_n_u8(0x0f)));
        let hi = vqtbl1q_u8(vld1q_u8(HI_LUT.as_ptr()), vshrq_n_u8(v, 4));
        let hit = vtstq_u8(lo, hi);
        let structural = neon_movemask(hit) as u32;

        ChunkMasks {
            newlines: lf | cr,
            structural,
            spaces,
            all_ascii,
        }
    }
}

#[inline]
fn count_spaces_swar(bytes: &[u8], mut pos: usize) -> usize {
    let start = pos;
    const SPACES: u64 = 0x2020_2020_2020_2020;
    while pos + 8 <= bytes.len() {
        let word = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
        let diff = word ^ SPACES;
        if diff != 0 {
            return (pos - start) + (diff.trailing_zeros() >> 3) as usize;
        }
        pos += 8;
    }
    while pos < bytes.len() && bytes[pos] == b' ' {
        pos += 1;
    }
    pos - start
}

fn scan_tail_scalar(slice: &[u8]) -> (ChunkMasks, usize) {
    let n = slice.len().min(32);
    let mut masks = ChunkMasks {
        newlines: 0,
        structural: 0,
        spaces: 0,
        all_ascii: true,
    };
    for (i, &b) in slice[..n].iter().enumerate() {
        let bit = 1u32 << i;
        if b >= 0x80 {
            masks.all_ascii = false;
        } else if b == b' ' {
            masks.spaces |= bit;
        } else if b == b'\n' || b == b'\r' {
            masks.newlines |= bit;
        } else if matches!(
            b,
            b':' | b'-'
                | b'['
                | b']'
                | b'{'
                | b'}'
                | b','
                | b'#'
                | b'"'
                | b'\''
                | b'&'
                | b'*'
                | b'!'
                | b'|'
                | b'>'
                | b'?'
                | b'%'
                | b'\\'
        ) {
            masks.structural |= bit;
        }
    }
    (masks, n)
}
