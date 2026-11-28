pub const FFMPEG_VERSION: &str = "8.1.2";
pub const LIBASS_VERSION: &str = "0.17.5";
pub const HARFBUZZ_VERSION: &str = "14.2.1";
pub const FREETYPE_VERSION: &str = "2.14.3";

#[allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    unnecessary_transmutes
)]
mod bindings {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

pub use bindings::*;

pub const ERIKA_SWS_BILINEAR: std::os::raw::c_int = SwsFlags_SWS_BILINEAR as std::os::raw::c_int;
pub const ERIKA_SWS_LANCZOS: std::os::raw::c_int = SwsFlags_SWS_LANCZOS as std::os::raw::c_int;
pub const ERIKA_PROFILE_UNKNOWN: i32 = AV_PROFILE_UNKNOWN;

#[cfg(test)]
mod tests {
    use super::{ERIKA_PROFILE_UNKNOWN, ERIKA_SWS_BILINEAR, ERIKA_SWS_LANCZOS};

    #[test]
    fn all_target_profiles_include_an_av1_cpu_decoder() {
        for profile in [
            super::NativeDependencyProfile::Lgpl,
            super::NativeDependencyProfile::GplFull,
        ] {
            for target_os in ["macos", "ios", "tvos", "windows", "android", "linux"] {
                let flags = profile.ffmpeg_configure_flags_for_target_os(target_os);
                assert!(flags.contains(&"--enable-libdav1d"));
                assert!(flags.contains(&"--enable-decoder=libdav1d"));
                assert_eq!(
                    flags.contains(&"--enable-videotoolbox"),
                    matches!(target_os, "macos" | "ios" | "tvos")
                );
                assert_eq!(
                    flags.contains(&"--enable-mediacodec"),
                    target_os == "android"
                );
            }
        }
    }

    #[test]
    fn ffmpeg_812_compatibility_constants() {
        assert_eq!(ERIKA_SWS_BILINEAR, 2);
        assert_eq!(ERIKA_SWS_LANCZOS, 512);
        assert_eq!(ERIKA_PROFILE_UNKNOWN, -99);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeDependencyProfile {
    Lgpl,
    GplFull,
}

impl NativeDependencyProfile {
    pub fn ffmpeg_configure_flags(self) -> &'static [&'static str] {
        match self {
            Self::Lgpl => &[
                "--disable-gpl",
                "--enable-version3",
                "--enable-static",
                "--disable-shared",
                "--disable-programs",
                "--disable-doc",
                "--disable-network",
                "--disable-autodetect",
                "--enable-zlib",
                "--enable-libdav1d",
                "--enable-decoder=libdav1d",
                "--enable-protocol=file",
                "--enable-demuxer=asf,av1,ivf,mov,matroska,mpegts,mp3,aac,flac,wav,ogg,ass,srt,webvtt",
                "--enable-parser=av1,hevc,h264,aac,opus,vorbis,flac,mpegaudio",
                "--enable-decoder=wmv1,wmv2,wmv3,vc1,wmav1,wmav2,wmapro,wmalossless,hevc,h264,aac,opus,vorbis,flac,mp3,pcm_s16le,pcm_s24le,pcm_s32le,ass,srt,webvtt",
                "--enable-encoder=gif",
                "--enable-muxer=gif",
            ],
            Self::GplFull => &[
                "--enable-gpl",
                "--enable-version3",
                "--enable-static",
                "--disable-shared",
                "--disable-programs",
                "--disable-doc",
                "--disable-network",
                "--disable-autodetect",
                "--enable-zlib",
                "--enable-libdav1d",
                "--enable-decoder=libdav1d",
                "--enable-protocol=file",
                "--enable-demuxer=asf,av1,ivf,mov,matroska,mpegts,mp3,aac,flac,wav,ogg,ass,srt,webvtt",
                "--enable-parser=av1,hevc,h264,aac,opus,vorbis,flac,mpegaudio",
                "--enable-decoder=wmv1,wmv2,wmv3,vc1,wmav1,wmav2,wmapro,wmalossless,hevc,h264,aac,opus,vorbis,flac,mp3,pcm_s16le,pcm_s24le,pcm_s32le,ass,srt,webvtt",
                "--enable-encoder=gif",
                "--enable-muxer=gif",
            ],
        }
    }

    pub fn ffmpeg_configure_flags_for_target_os(self, target_os: &str) -> Vec<&'static str> {
        let mut flags = self.ffmpeg_configure_flags().to_vec();
        match target_os {
            "macos" | "ios" | "tvos" => flags.push("--enable-videotoolbox"),
            "windows" => flags.extend(["--enable-d3d11va", "--enable-dxva2"]),
            "android" => flags.extend([
                "--enable-jni",
                "--enable-mediacodec",
                "--enable-decoder=h264_mediacodec,hevc_mediacodec,mpeg2_mediacodec,mpeg4_mediacodec,vp8_mediacodec,vp9_mediacodec,av1_mediacodec",
            ]),
            _ => {}
        }
        flags
    }
}

#[cfg(test)]
mod native_dependency_profile_tests {
    use super::NativeDependencyProfile;

    #[test]
    fn every_profile_enables_gif_export_components() {
        for profile in [
            NativeDependencyProfile::Lgpl,
            NativeDependencyProfile::GplFull,
        ] {
            let flags = profile.ffmpeg_configure_flags();
            assert!(flags.contains(&"--enable-encoder=gif"));
            assert!(flags.contains(&"--enable-muxer=gif"));
        }
    }
}
