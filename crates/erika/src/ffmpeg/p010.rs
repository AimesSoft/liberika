//! Byte-oriented row kernels for planar 10-bit LE to P010 conversion.
//!
//! Keep frame pointers, strides and allocation in `ffmpeg.rs`. These kernels also
//! compile directly with rustc, without FFmpeg, for regression tests and timing.

/// Converts one complete row, including rows whose address is not u16-aligned.
#[inline]
pub(super) fn luma_row(src: &[u8], dst: &mut [u8]) {
    debug_assert_eq!(src.len(), dst.len());
    debug_assert_eq!(src.len() % 2, 0);
    let mut src_blocks = src.chunks_exact(8);
    let mut dst_blocks = dst.chunks_exact_mut(8);
    for (sample, dest) in src_blocks.by_ref().zip(dst_blocks.by_ref()) {
        // Four independent 16-bit lanes. Mask before shifting so high source
        // bits cannot spill into the next lane. LE byte loads need no alignment.
        let value = (u64::from_le_bytes(sample.try_into().unwrap()) & 0x03ff_03ff_03ff_03ff) << 6;
        dest.copy_from_slice(&value.to_le_bytes());
    }
    for (sample, dest) in src_blocks
        .remainder()
        .chunks_exact(2)
        .zip(dst_blocks.into_remainder().chunks_exact_mut(2))
    {
        let value = (u16::from_le_bytes([sample[0], sample[1]]) & 0x03ff) << 6;
        dest.copy_from_slice(&value.to_le_bytes());
    }
}

/// Interleaves one complete pair of Cb/Cr rows, preserving all ten bits.
#[inline]
pub(super) fn chroma_row(u: &[u8], v: &[u8], dst: &mut [u8]) {
    #[cfg(target_arch = "x86_64")]
    chroma_row_sse2(u, v, dst);
    #[cfg(not(target_arch = "x86_64"))]
    chroma_row_scalar(u, v, dst);
}

#[inline]
pub(super) fn chroma_row_scalar(u: &[u8], v: &[u8], dst: &mut [u8]) {
    debug_assert_eq!(u.len(), v.len());
    debug_assert_eq!(u.len() * 2, dst.len());
    debug_assert_eq!(u.len() % 2, 0);
    // Retain this loop: it already vectorizes on arm64; tested portable block
    // alternatives did not improve it. No optional CPU features are required.
    for ((u, v), dest) in u
        .chunks_exact(2)
        .zip(v.chunks_exact(2))
        .zip(dst.chunks_exact_mut(4))
    {
        let u = (u16::from_le_bytes([u[0], u[1]]) & 0x03ff) << 6;
        let v = (u16::from_le_bytes([v[0], v[1]]) & 0x03ff) << 6;
        dest[..2].copy_from_slice(&u.to_le_bytes());
        dest[2..].copy_from_slice(&v.to_le_bytes());
    }
}

#[cfg(target_arch = "x86_64")]
pub(super) fn chroma_row_sse2(u: &[u8], v: &[u8], dst: &mut [u8]) {
    use std::arch::x86_64::*;
    assert_eq!(u.len(), v.len());
    assert_eq!(u.len() % 2, 0);
    assert_eq!(u.len().checked_mul(2), Some(dst.len()));
    let end = u.len() / 16 * 16;
    for offset in (0..end).step_by(16) {
        // Bounds follow from `end` and the length assertions. Unaligned loads
        // accept odd plane addresses. Shifting each 16-bit lane discards the
        // high six source bits, exactly like `(sample & 0x03ff) << 6`.
        unsafe {
            let uu = _mm_slli_epi16::<6>(_mm_loadu_si128(u.as_ptr().add(offset).cast()));
            let vv = _mm_slli_epi16::<6>(_mm_loadu_si128(v.as_ptr().add(offset).cast()));
            _mm_storeu_si128(
                dst.as_mut_ptr().add(offset * 2).cast(),
                _mm_unpacklo_epi16(uu, vv),
            );
            _mm_storeu_si128(
                dst.as_mut_ptr().add(offset * 2 + 16).cast(),
                _mm_unpackhi_epi16(uu, vv),
            );
        }
    }
    chroma_row_scalar(&u[end..], &v[end..], &mut dst[end * 2..]);
}
