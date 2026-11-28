//! Interpret the native buffer format, not just AVCodec's NV12 alias.
//! Some devices report pixel_format=NV12 for both NV12 and NV12_10bit.

const AV_PIXEL_FORMAT_NV12: i32 = 2;
const NATIVE_FORMAT_NV12: i32 = 24;
const NATIVE_FORMAT_P010: i32 = 35;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoPixelFormat {
    Nv12,
    P010,
}

impl VideoPixelFormat {
    pub fn from_native_buffer(av_format: i32, native_format: Option<i32>) -> Result<Self, String> {
        match native_format {
            Some(NATIVE_FORMAT_NV12) => Ok(Self::Nv12),
            Some(NATIVE_FORMAT_P010) => Ok(Self::P010),
            None if av_format == AV_PIXEL_FORMAT_NV12 => Ok(Self::Nv12),
            _ => Err(format!(
                "unsupported AVCodec buffer format: metadata={av_format}, native={native_format:?}"
            )),
        }
    }

    pub const fn bytes_per_sample(self) -> usize {
        match self {
            Self::Nv12 => 1,
            Self::P010 => 2,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Nv12 => "nv12",
            Self::P010 => "p010le",
        }
    }

    pub fn luma_row_bytes(self, width: usize) -> Result<usize, String> {
        width
            .checked_mul(self.bytes_per_sample())
            .ok_or_else(|| "AVCodec luma row overflowed".into())
    }

    pub fn chroma_row_bytes(self, width: usize) -> Result<usize, String> {
        width
            .div_ceil(2)
            .checked_mul(2 * self.bytes_per_sample())
            .ok_or_else(|| "AVCodec chroma row overflowed".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main10_native_format_overrides_nv12_metadata_alias() {
        let format = VideoPixelFormat::from_native_buffer(2, Some(35)).unwrap();
        assert_eq!(format, VideoPixelFormat::P010);
        assert_eq!(format.luma_row_bytes(1920).unwrap(), 3840);
        assert_eq!(format.chroma_row_bytes(1920).unwrap(), 3840);
    }

    #[test]
    fn eight_bit_native_nv12_keeps_single_byte_samples() {
        let format = VideoPixelFormat::from_native_buffer(2, Some(24)).unwrap();
        assert_eq!(format, VideoPixelFormat::Nv12);
        assert_eq!(format.luma_row_bytes(1920).unwrap(), 1920);
        assert_eq!(format.chroma_row_bytes(1920).unwrap(), 1920);
    }

    #[test]
    fn shared_memory_nv12_without_native_buffer_is_supported() {
        assert_eq!(
            VideoPixelFormat::from_native_buffer(2, None).unwrap(),
            VideoPixelFormat::Nv12
        );
    }

    #[test]
    fn unsupported_native_formats_do_not_fall_through_to_nv12() {
        for format in [0, 25, 36, 0x7fff0000] {
            assert!(VideoPixelFormat::from_native_buffer(2, Some(format)).is_err());
        }
        assert!(VideoPixelFormat::from_native_buffer(4, None).is_err());
    }

    #[test]
    fn odd_width_chroma_rounds_up_in_samples_before_bytes() {
        assert_eq!(VideoPixelFormat::P010.luma_row_bytes(3).unwrap(), 6);
        assert_eq!(VideoPixelFormat::P010.chroma_row_bytes(3).unwrap(), 8);
        assert_eq!(VideoPixelFormat::Nv12.chroma_row_bytes(3).unwrap(), 4);
    }

    #[test]
    fn row_arithmetic_rejects_overflow() {
        assert!(VideoPixelFormat::P010.luma_row_bytes(usize::MAX).is_err());
        assert!(VideoPixelFormat::P010.chroma_row_bytes(usize::MAX).is_err());
    }
}
