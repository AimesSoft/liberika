use std::time::{Duration, Instant};

use ab_glyph::{Font, FontArc, Glyph, PxScale, ScaleFont, point};

use crate::subtitle::SubtitleBitmapPlane;

const REFRESH_INTERVAL: Duration = Duration::from_millis(500);
const FONT_SIZE: f32 = 22.0;
const LINE_HEIGHT: u32 = 29;
const PADDING: u32 = 14;
const MAX_WIDTH: u32 = 820;

#[derive(Debug, Clone, PartialEq)]
pub struct DebugHudSnapshot {
    pub codec: Option<String>,
    pub width: u32,
    pub height: u32,
    pub bit_rate: Option<u64>,
    pub nominal_fps: Option<f64>,
    pub pixel_format: Option<String>,
    pub profile: Option<String>,
    pub decoder_requested_backend: Option<String>,
    pub decoder_previous_backend: Option<String>,
    pub decoder_active_backend: Option<String>,
    pub decoder_codec: Option<String>,
    pub decoder_pixel_format: Option<String>,
    pub decoder_line_sizes: Option<[i32; 4]>,
    pub decoder_fallback_count: u64,
    pub decoder_stage: Option<String>,
    pub decoder_reason: Option<String>,
    pub player_state: String,
    pub media_time: Duration,
    pub duration: Option<Duration>,
    pub playback_rate: f64,
    pub surface_width: u32,
    pub surface_height: u32,
    pub decoded_video_frames: u64,
    pub rendered_video_frames: u64,
    pub dropped_video_frames: u64,
    pub hardware_video_frames: u64,
    pub software_video_frames: u64,
    pub zero_copy_video_frames: u64,
    pub direct_zero_copy_video_frames: u64,
    pub shared_handle_video_frames: u64,
    pub cpu_video_frame_fallbacks: u64,
    pub import_failures: u64,
    pub render_failures: u64,
    pub render_duration: Duration,
    pub gpu_duration: Duration,
    pub audio_queued_frames: usize,
    pub audio_queued_duration: Option<Duration>,
    pub audio_underflow_frames: u64,
    pub audio_codec: Option<String>,
    pub audio_sample_rate: u32,
    pub audio_channels: u32,
    pub audio_recovery_state: String,
    pub hdr_output_active: bool,
    pub output_encoding: String,
    pub output_format: String,
    pub output_headroom: f32,
    pub output_fallback: String,
    pub danmaku_items: usize,
}

#[derive(Debug)]
pub struct DebugHud {
    enabled: bool,
    font: Option<FontArc>,
    last_refresh: Option<Instant>,
    previous_sample: Option<(Instant, u64, u64)>,
    plane: Option<SubtitleBitmapPlane>,
}

impl DebugHud {
    pub fn new() -> Self {
        Self {
            enabled: false,
            font: crate::danmaku::load_default_font(),
            last_refresh: None,
            previous_sample: None,
            plane: None,
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        self.last_refresh = None;
        self.previous_sample = None;
        self.plane = None;
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn update(
        &mut self,
        now: Instant,
        viewport_width: u32,
        viewport_height: u32,
        snapshot: DebugHudSnapshot,
    ) -> Option<&SubtitleBitmapPlane> {
        if !self.enabled || viewport_width == 0 || viewport_height == 0 {
            return None;
        }
        if self
            .last_refresh
            .is_some_and(|last| now.saturating_duration_since(last) < REFRESH_INTERVAL)
        {
            return self.plane.as_ref();
        }

        let (decoded_fps, rendered_fps) = self.sample_fps(now, &snapshot);
        let lines = hud_lines(&snapshot, decoded_fps, rendered_fps);
        self.plane = self
            .font
            .as_ref()
            .and_then(|font| render_lines(font, &lines, viewport_width, viewport_height));
        self.last_refresh = Some(now);
        self.plane.as_ref()
    }

    fn sample_fps(&mut self, now: Instant, snapshot: &DebugHudSnapshot) -> (f64, f64) {
        let rates = self.previous_sample.and_then(|(last, decoded, rendered)| {
            let elapsed = now.saturating_duration_since(last).as_secs_f64();
            (elapsed > 0.0).then(|| {
                (
                    snapshot.decoded_video_frames.saturating_sub(decoded) as f64 / elapsed,
                    snapshot.rendered_video_frames.saturating_sub(rendered) as f64 / elapsed,
                )
            })
        });
        self.previous_sample = Some((
            now,
            snapshot.decoded_video_frames,
            snapshot.rendered_video_frames,
        ));
        rates.unwrap_or((0.0, 0.0))
    }
}

impl Default for DebugHud {
    fn default() -> Self {
        Self::new()
    }
}

fn hud_lines(snapshot: &DebugHudSnapshot, decoded_fps: f64, rendered_fps: f64) -> Vec<String> {
    let codec = snapshot
        .codec
        .as_deref()
        .unwrap_or("unknown")
        .to_uppercase();
    let nominal_fps = snapshot
        .nominal_fps
        .map(|fps| format!("{fps:.2}"))
        .unwrap_or_else(|| "n/a".to_string());
    let bit_rate = snapshot
        .bit_rate
        .map(format_bit_rate)
        .unwrap_or_else(|| "n/a".to_string());
    let decode_path = if snapshot.hardware_video_frames > snapshot.software_video_frames {
        "hardware"
    } else if snapshot.software_video_frames > 0 {
        "software"
    } else {
        "pending"
    };
    let pixel_format = snapshot.pixel_format.as_deref().unwrap_or("unknown");
    let profile = snapshot.profile.as_deref().unwrap_or("unknown");
    let decoder_requested = snapshot
        .decoder_requested_backend
        .as_deref()
        .unwrap_or("pending");
    let decoder_previous = snapshot
        .decoder_previous_backend
        .as_deref()
        .unwrap_or("none");
    let decoder_active = snapshot
        .decoder_active_backend
        .as_deref()
        .unwrap_or("pending");
    let decoder_codec = snapshot.decoder_codec.as_deref().unwrap_or("unknown");
    let decoder_pixel_format = snapshot
        .decoder_pixel_format
        .as_deref()
        .unwrap_or("unknown");
    let decoder_line_sizes = snapshot
        .decoder_line_sizes
        .map(|sizes| format!("{}/{}/{}/{}", sizes[0], sizes[1], sizes[2], sizes[3]))
        .unwrap_or_else(|| "n/a".to_string());
    let decoder_stage = snapshot.decoder_stage.as_deref().unwrap_or("pending");
    let decoder_reason = snapshot.decoder_reason.as_deref().unwrap_or("none");
    let audio_codec = snapshot.audio_codec.as_deref().unwrap_or("unknown");
    let audio_queue_ms = snapshot
        .audio_queued_duration
        .map(|duration| duration.as_secs_f64() * 1000.0)
        .unwrap_or(0.0);
    vec![
        format!(
            "ERIKA HUD  {codec}  {}x{}  {nominal_fps} fps  {bit_rate}",
            snapshot.width, snapshot.height
        ),
        format!(
            "State {}  Time {} / {}  Speed {:.2}x  Surface {}x{}",
            snapshot.player_state,
            format_duration(snapshot.media_time),
            snapshot
                .duration
                .map(format_duration)
                .unwrap_or_else(|| "--:--".to_string()),
            snapshot.playback_rate,
            snapshot.surface_width,
            snapshot.surface_height
        ),
        format!("Video profile {profile}  Format {pixel_format}  Path {decode_path}"),
        format!(
            "Decoder requested {decoder_requested}  Previous {decoder_previous}  Active {decoder_active}",
        ),
        format!(
            "Decoder codec {decoder_codec}  Format {decoder_pixel_format}  Lines {decoder_line_sizes}",
        ),
        format!(
            "Decoder fallback {}  Stage {}",
            snapshot.decoder_fallback_count, decoder_stage,
        ),
        format!("Decoder reason {}", truncate_decoder_reason(decoder_reason)),
        format!(
            "Decode {decoded_fps:5.1} fps ({})  Render {rendered_fps:5.1} fps ({})  Drop {}",
            snapshot.decoded_video_frames,
            snapshot.rendered_video_frames,
            snapshot.dropped_video_frames,
        ),
        format!(
            "Zero-copy {}  Direct {}  Shared {}  CPU fallback {}",
            snapshot.zero_copy_video_frames,
            snapshot.direct_zero_copy_video_frames,
            snapshot.shared_handle_video_frames,
            snapshot.cpu_video_frame_fallbacks,
        ),
        format!(
            "Render {:5.2} ms  GPU {:5.2} ms  Import fail {}  Render fail {}",
            snapshot.render_duration.as_secs_f64() * 1000.0,
            snapshot.gpu_duration.as_secs_f64() * 1000.0,
            snapshot.import_failures,
            snapshot.render_failures,
        ),
        format!(
            "Audio {audio_codec}  {} Hz  {} ch  Queue {} / {:.1} ms  Underflow {}  {}",
            snapshot.audio_sample_rate,
            snapshot.audio_channels,
            snapshot.audio_queued_frames,
            audio_queue_ms,
            snapshot.audio_underflow_frames,
            snapshot.audio_recovery_state,
        ),
        format!(
            "Output {}  {}  Headroom {:.2}x  HDR {}  Fallback {}  Danmaku {}",
            snapshot.output_encoding,
            snapshot.output_format,
            snapshot.output_headroom,
            if snapshot.hdr_output_active {
                "on"
            } else {
                "off"
            },
            snapshot.output_fallback,
            snapshot.danmaku_items,
        ),
    ]
}

fn truncate_decoder_reason(reason: &str) -> String {
    const MAX_CHARS: usize = 72;
    let mut chars = reason.chars();
    let prefix = chars.by_ref().take(MAX_CHARS).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

fn format_duration(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3_600;
    let minutes = total_seconds % 3_600 / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn format_bit_rate(bit_rate: u64) -> String {
    if bit_rate >= 1_000_000 {
        format!("{:.2} Mbps", bit_rate as f64 / 1_000_000.0)
    } else if bit_rate >= 1_000 {
        format!("{:.1} Kbps", bit_rate as f64 / 1_000.0)
    } else {
        format!("{bit_rate} bps")
    }
}

fn render_lines(
    font: &FontArc,
    lines: &[String],
    viewport_width: u32,
    viewport_height: u32,
) -> Option<SubtitleBitmapPlane> {
    let width = MAX_WIDTH.min(viewport_width.saturating_sub(PADDING * 2));
    let height = (PADDING * 2 + LINE_HEIGHT * lines.len() as u32)
        .min(viewport_height.saturating_sub(PADDING * 2));
    if width == 0 || height == 0 {
        return None;
    }
    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.copy_from_slice(&[8, 10, 14, 190]);
    }
    let scale = PxScale::from(FONT_SIZE);
    let scaled = font.as_scaled(scale);
    for (line_index, line) in lines.iter().enumerate() {
        let mut cursor = PADDING as f32;
        let baseline = PADDING as f32 + FONT_SIZE + line_index as f32 * LINE_HEIGHT as f32;
        let mut previous = None;
        for character in line.chars() {
            let glyph_id = scaled.glyph_id(character);
            if let Some(previous) = previous {
                cursor += scaled.kern(previous, glyph_id);
            }
            let glyph = Glyph {
                id: glyph_id,
                scale,
                position: point(cursor, baseline),
            };
            if let Some(outlined) = font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                outlined.draw(|x, y, coverage| {
                    let px = bounds.min.x.floor() as i32 + x as i32;
                    let py = bounds.min.y.floor() as i32 + y as i32;
                    if px < 0 || py < 0 || px >= width as i32 || py >= height as i32 {
                        return;
                    }
                    let index = (py as usize * width as usize + px as usize) * 4;
                    blend_source_over(&mut rgba[index..index + 4], [238, 244, 255], coverage);
                });
            }
            cursor += scaled.h_advance(glyph_id);
            previous = Some(glyph_id);
            if cursor >= width.saturating_sub(PADDING) as f32 {
                break;
            }
        }
    }
    Some(SubtitleBitmapPlane::new(
        PADDING as i32,
        PADDING as i32,
        width,
        height,
        rgba,
    ))
}

fn blend_source_over(destination: &mut [u8], source_rgb: [u8; 3], coverage: f32) {
    let source_alpha = coverage.clamp(0.0, 1.0);
    if source_alpha <= 0.0 {
        return;
    }
    let destination_alpha = f32::from(destination[3]) / 255.0;
    let output_alpha = source_alpha + destination_alpha * (1.0 - source_alpha);
    for channel in 0..3 {
        let source = f32::from(source_rgb[channel]) / 255.0;
        let destination_color = f32::from(destination[channel]) / 255.0;
        let output = (source * source_alpha
            + destination_color * destination_alpha * (1.0 - source_alpha))
            / output_alpha;
        destination[channel] = (output.clamp(0.0, 1.0) * 255.0).round() as u8;
    }
    destination[3] = (output_alpha * 255.0).round() as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    use ab_glyph::{Font, FontArc};

    use crate::NIPAPLAY_FALLBACK_FONT;

    fn snapshot(decoded: u64, rendered: u64) -> DebugHudSnapshot {
        DebugHudSnapshot {
            codec: Some("hevc".to_string()),
            width: 3840,
            height: 2160,
            bit_rate: Some(18_000_000),
            nominal_fps: Some(30_000.0 / 1_001.0),
            pixel_format: Some("yuv420p10le".to_string()),
            profile: Some("Main 10".to_string()),
            decoder_requested_backend: Some("videotoolbox".to_string()),
            decoder_previous_backend: Some("videotoolbox".to_string()),
            decoder_active_backend: Some("software".to_string()),
            decoder_codec: Some("hevc".to_string()),
            decoder_pixel_format: Some("yuv420p10le".to_string()),
            decoder_line_sizes: Some([7680, 3840, 3840, 0]),
            decoder_fallback_count: 1,
            decoder_stage: Some("runtime_to_software".to_string()),
            decoder_reason: Some("hardware import failed".to_string()),
            player_state: "playing".to_string(),
            media_time: Duration::from_secs(62),
            duration: Some(Duration::from_secs(3_725)),
            playback_rate: 1.25,
            surface_width: 1920,
            surface_height: 1080,
            decoded_video_frames: decoded,
            rendered_video_frames: rendered,
            dropped_video_frames: 2,
            hardware_video_frames: 10,
            software_video_frames: 0,
            zero_copy_video_frames: 9,
            direct_zero_copy_video_frames: 8,
            shared_handle_video_frames: 1,
            cpu_video_frame_fallbacks: 2,
            import_failures: 3,
            render_failures: 4,
            render_duration: Duration::from_micros(800),
            gpu_duration: Duration::from_micros(500),
            audio_queued_frames: 512,
            audio_queued_duration: Some(Duration::from_millis(12)),
            audio_underflow_frames: 1,
            audio_codec: Some("aac".to_string()),
            audio_sample_rate: 48_000,
            audio_channels: 2,
            audio_recovery_state: "stable".to_string(),
            hdr_output_active: true,
            output_encoding: "hdr10-pq".to_string(),
            output_format: "10bit-unorm".to_string(),
            output_headroom: 4.0,
            output_fallback: "none".to_string(),
            danmaku_items: 16,
        }
    }

    #[test]
    fn hud_is_disabled_by_default_and_reuses_cached_plane() {
        let mut hud = DebugHud::new();
        let now = Instant::now();
        assert!(hud.update(now, 1920, 1080, snapshot(1, 1)).is_none());
        hud.set_enabled(true);
        let first = hud.update(now, 1920, 1080, snapshot(1, 1)).cloned();
        let second = hud
            .update(now + Duration::from_millis(100), 1920, 1080, snapshot(5, 5))
            .cloned();
        assert!(first.is_some());
        assert_eq!(first, second);
    }

    #[test]
    fn hud_text_contains_media_and_runtime_metrics() {
        let lines = hud_lines(&snapshot(30, 29), 30.0, 29.0);
        assert!(lines[0].contains("HEVC"));
        assert!(lines[0].contains("3840x2160"));
        assert!(lines[0].contains("18.00 Mbps"));
        assert!(lines[1].contains("01:02 / 1:02:05"));
        assert!(lines[2].contains("yuv420p10le"));
        assert!(lines[3].contains("requested videotoolbox"));
        assert!(lines[3].contains("Active software"));
        assert!(lines[4].contains("Lines 7680/3840/3840/0"));
        assert!(lines[5].contains("runtime_to_software"));
        assert!(lines[7].contains("Decode  30.0 fps"));
        assert!(lines[8].contains("Direct 8"));
        assert!(lines[9].contains("Import fail 3"));
        assert!(lines[10].contains("aac"));
        assert!(lines[10].contains("Underflow 1"));
        assert!(lines[11].contains("hdr10-pq"));
        assert!(lines[11].contains("Danmaku 16"));
    }

    #[test]
    fn larger_hud_allocates_all_diagnostic_rows() {
        let font = FontArc::try_from_slice(NIPAPLAY_FALLBACK_FONT).unwrap();
        let lines = hud_lines(&snapshot(30, 29), 30.0, 29.0);
        let plane = render_lines(&font, &lines, 1920, 1080).unwrap();
        assert_eq!(plane.width, MAX_WIDTH);
        assert_eq!(plane.height, PADDING * 2 + LINE_HEIGHT * lines.len() as u32);
        assert_eq!(FONT_SIZE, 22.0);
    }

    #[test]
    fn bundled_font_contains_hud_ascii_glyphs() {
        let font = FontArc::try_from_slice(NIPAPLAY_FALLBACK_FONT).unwrap();
        for character in "ERIKA HUD Decode Render Path Zero-copy GPU Audio 0123456789.-/".chars() {
            if character.is_whitespace() {
                continue;
            }
            assert_ne!(font.glyph_id(character).0, 0, "missing glyph {character:?}");
        }
    }

    #[test]
    fn antialiased_glyph_coverage_blends_over_background() {
        let mut low = [8, 10, 14, 190];
        let mut full = low;
        blend_source_over(&mut low, [238, 244, 255], 0.1);
        blend_source_over(&mut full, [238, 244, 255], 1.0);
        assert!(low[0] < full[0]);
        assert!(low[3] < full[3]);
        assert_eq!(full, [238, 244, 255, 255]);
    }
}
