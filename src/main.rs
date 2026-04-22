use std::{
    collections::VecDeque,
    error::Error,
    fs::{self, File},
    io::BufWriter,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use image::{
    Delay, DynamicImage, Frame, RgbImage, RgbaImage,
    codecs::gif::{GifEncoder, Repeat},
    imageops::FilterType,
};
use nokhwa::{
    Camera, native_api_backend, nokhwa_check, nokhwa_initialize,
    pixel_format::RgbFormat,
    query,
    utils::{CameraIndex, RequestedFormat, RequestedFormatType},
};

fn main() -> Result<(), Box<dyn Error>> {
    let args = read_args()?;
    wait_for_camera_permission()?;

    let backend =
        native_api_backend().ok_or("No native camera backend is available on this OS.")?;
    let cameras = query(backend)?;

    if cameras.is_empty() {
        return Err("No webcams were found.".into());
    }

    let camera_index = args.camera_index;
    if camera_index as usize >= cameras.len() {
        return Err(format!(
            "Camera index {camera_index} is out of range. Found {} camera(s): choose 0 through {}.",
            cameras.len(),
            cameras.len() - 1
        )
        .into());
    }

    println!("Found {} camera(s) using {backend:?}.", cameras.len());
    println!("Opening camera index {camera_index} on a dedicated capture thread.");

    let (receiver, capture_handle) = spawn_capture_thread(camera_index);

    let run_result = (|| -> Result<(), Box<dyn Error>> {
        let capture_info = wait_for_capture_start(&receiver)?;
        println!(
            "Resolved camera format: {}x{} at {} FPS.",
            capture_info.width, capture_info.height, capture_info.frame_rate
        );

        run_motion_detection(
            receiver,
            MotionDetectorConfig::default(),
            ClipRecorderConfig::default(),
            args.max_frames,
            capture_info,
        )
    })();

    if capture_handle.join().is_err() {
        return Err("The capture thread panicked.".into());
    }

    run_result?;

    println!("Motion detection loop finished cleanly.");
    Ok(())
}

struct AppArgs {
    camera_index: u32,
    max_frames: Option<usize>,
}

fn read_args() -> Result<AppArgs, Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let camera_index = match args.next() {
        Some(value) => value
            .parse()
            .map_err(|_| format!("Invalid camera index: {value}"))?,
        None => 0,
    };

    let max_frames = match args.next() {
        Some(value) => Some(
            value
                .parse()
                .map_err(|_| format!("Invalid frame limit: {value}"))?,
        ),
        None => None,
    };

    if args.next().is_some() {
        return Err("Usage: cargo run -- [camera_index] [max_frames]".into());
    }

    Ok(AppArgs {
        camera_index,
        max_frames,
    })
}

fn wait_for_camera_permission() -> Result<(), Box<dyn Error>> {
    if !cfg!(target_os = "macos") || nokhwa_check() {
        return Ok(());
    }

    println!("macOS needs camera access before capture can start.");
    let permission_result = Arc::new(Mutex::new(None));
    let permission_result_for_callback = Arc::clone(&permission_result);

    nokhwa_initialize(move |granted| {
        if let Ok(mut slot) = permission_result_for_callback.lock() {
            *slot = Some(granted);
        }
    });

    let started_waiting = Instant::now();
    loop {
        if let Some(granted) = *permission_result
            .lock()
            .map_err(|_| "Permission lock poisoned.")?
        {
            if granted {
                return Ok(());
            }

            return Err("Camera permission was denied in macOS.".into());
        }

        if started_waiting.elapsed() > Duration::from_secs(30) {
            return Err("Timed out while waiting for macOS camera permission.".into());
        }

        thread::sleep(Duration::from_millis(50));
    }
}

#[derive(Clone, Copy)]
struct CaptureInfo {
    width: u32,
    height: u32,
    frame_rate: u32,
}

struct CapturedFrame {
    frame_number: usize,
    rgb: RgbImage,
}

enum CaptureMessage {
    Started(CaptureInfo),
    Frame(CapturedFrame),
    Error(String),
}

fn spawn_capture_thread(camera_index: u32) -> (Receiver<CaptureMessage>, thread::JoinHandle<()>) {
    let (sender, receiver) = sync_channel(8);

    let handle = thread::spawn(move || {
        if let Err(error) = capture_loop(camera_index, &sender) {
            let _ = sender.send(CaptureMessage::Error(error.to_string()));
        }
    });

    (receiver, handle)
}

fn wait_for_capture_start(
    receiver: &Receiver<CaptureMessage>,
) -> Result<CaptureInfo, Box<dyn Error>> {
    loop {
        match receiver
            .recv()
            .map_err(|_| "The capture thread exited before sending startup info.")?
        {
            CaptureMessage::Started(info) => return Ok(info),
            CaptureMessage::Error(message) => return Err(message.into()),
            CaptureMessage::Frame(_) => {
                return Err("The capture thread sent a frame before startup info.".into());
            }
        }
    }
}

fn capture_loop(
    camera_index: u32,
    sender: &SyncSender<CaptureMessage>,
) -> Result<(), Box<dyn Error>> {
    let backend =
        native_api_backend().ok_or("No native camera backend is available on this OS.")?;
    let requested =
        RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestFrameRate);
    let mut camera = Camera::with_backend(CameraIndex::Index(camera_index), requested, backend)?;
    let resolution = camera.resolution();
    let capture_info = CaptureInfo {
        width: resolution.width(),
        height: resolution.height(),
        frame_rate: camera.frame_rate(),
    };

    camera.open_stream()?;

    if sender.send(CaptureMessage::Started(capture_info)).is_err() {
        let _ = camera.stop_stream();
        return Ok(());
    }

    let mut frame_number = 0usize;
    loop {
        frame_number += 1;
        let frame = camera.frame()?;
        let rgb = frame.decode_image::<RgbFormat>()?;

        if sender
            .send(CaptureMessage::Frame(CapturedFrame { frame_number, rgb }))
            .is_err()
        {
            break;
        }
    }

    let _ = camera.stop_stream();
    Ok(())
}

#[derive(Clone, Copy)]
struct MotionDetectorConfig {
    sample_width: usize,
    sample_height: usize,
    pixel_diff_threshold: u8,
    min_changed_pixels: usize,
    report_every_n_frames: usize,
}

impl Default for MotionDetectorConfig {
    fn default() -> Self {
        Self {
            sample_width: 64,
            sample_height: 48,
            pixel_diff_threshold: 28,
            min_changed_pixels: 180,
            report_every_n_frames: 30,
        }
    }
}

struct MotionDetector {
    config: MotionDetectorConfig,
    previous_sample: Option<Vec<u8>>,
    motion_active: bool,
}

impl MotionDetector {
    fn new(config: MotionDetectorConfig) -> Self {
        Self {
            config,
            previous_sample: None,
            motion_active: false,
        }
    }

    fn analyze_frame(&mut self, sampled_grayscale_pixels: Vec<u8>) -> MotionAnalysis {
        let total_pixels = sampled_grayscale_pixels.len();
        let Some(previous_sample) = self
            .previous_sample
            .replace(sampled_grayscale_pixels.clone())
        else {
            return MotionAnalysis {
                changed_pixels: 0,
                total_pixels,
                motion_detected: false,
                motion_started: false,
                motion_ended: false,
                warming_up: true,
            };
        };

        let changed_pixels = count_changed_pixels(
            &previous_sample,
            &sampled_grayscale_pixels,
            self.config.pixel_diff_threshold,
        );
        let motion_detected = changed_pixels >= self.config.min_changed_pixels;
        let motion_started = motion_detected && !self.motion_active;
        let motion_ended = !motion_detected && self.motion_active;
        self.motion_active = motion_detected;

        MotionAnalysis {
            changed_pixels,
            total_pixels,
            motion_detected,
            motion_started,
            motion_ended,
            warming_up: false,
        }
    }
}

struct MotionAnalysis {
    changed_pixels: usize,
    total_pixels: usize,
    motion_detected: bool,
    motion_started: bool,
    motion_ended: bool,
    warming_up: bool,
}

#[derive(Clone)]
struct ClipRecorderConfig {
    output_dir: PathBuf,
    max_clip_width: u32,
    target_save_fps: u32,
    pre_roll_duration: Duration,
    post_roll_duration: Duration,
    max_clip_duration: Duration,
}

impl Default for ClipRecorderConfig {
    fn default() -> Self {
        Self {
            output_dir: PathBuf::from("clips"),
            max_clip_width: 320,
            target_save_fps: 8,
            pre_roll_duration: Duration::from_millis(500),
            post_roll_duration: Duration::from_millis(500),
            max_clip_duration: Duration::from_secs(12),
        }
    }
}

#[derive(Clone)]
struct ClipFrame {
    image: RgbaImage,
}

struct ActiveClip {
    started_at_frame: usize,
    still_frames: usize,
    frames: Vec<ClipFrame>,
}

struct ClipRecorder {
    output_dir: PathBuf,
    clip_width: u32,
    clip_height: u32,
    frame_stride: usize,
    effective_fps: u32,
    pre_roll_frames: usize,
    post_roll_frames: usize,
    max_clip_frames: usize,
    recent_frames: VecDeque<ClipFrame>,
    active_clip: Option<ActiveClip>,
    next_clip_number: usize,
}

impl ClipRecorder {
    fn new(
        config: ClipRecorderConfig,
        source_width: u32,
        source_height: u32,
        source_fps: u32,
    ) -> Result<Self, Box<dyn Error>> {
        fs::create_dir_all(&config.output_dir)?;

        let clip_width = config.max_clip_width.min(source_width).max(1);
        let clip_height = ((u64::from(source_height) * u64::from(clip_width))
            / u64::from(source_width.max(1)))
        .max(1) as u32;

        let source_fps = source_fps.max(1);
        let frame_stride = usize::try_from(source_fps.div_ceil(config.target_save_fps.max(1)))
            .unwrap_or(1)
            .max(1);
        let effective_fps = (source_fps / frame_stride as u32).max(1);
        let pre_roll_frames = duration_to_frame_count(config.pre_roll_duration, effective_fps);
        let post_roll_frames = duration_to_frame_count(config.post_roll_duration, effective_fps);
        let max_clip_frames = duration_to_frame_count(config.max_clip_duration, effective_fps);

        Ok(Self {
            output_dir: config.output_dir,
            clip_width,
            clip_height,
            frame_stride,
            effective_fps,
            pre_roll_frames,
            post_roll_frames,
            max_clip_frames,
            recent_frames: VecDeque::new(),
            active_clip: None,
            next_clip_number: 1,
        })
    }

    fn clip_width(&self) -> u32 {
        self.clip_width
    }

    fn clip_height(&self) -> u32 {
        self.clip_height
    }

    fn effective_fps(&self) -> u32 {
        self.effective_fps
    }

    fn pre_roll_frames(&self) -> usize {
        self.pre_roll_frames
    }

    fn post_roll_frames(&self) -> usize {
        self.post_roll_frames
    }

    fn output_dir(&self) -> &Path {
        &self.output_dir
    }

    fn record_frame(
        &mut self,
        rgb_frame: &RgbImage,
        frame_number: usize,
        analysis: &MotionAnalysis,
    ) -> Result<Option<PathBuf>, Box<dyn Error>> {
        if frame_number % self.frame_stride != 0 {
            return Ok(None);
        }

        let clip_frame = ClipFrame {
            image: resize_for_clip(rgb_frame, self.clip_width, self.clip_height),
        };

        self.recent_frames.push_back(clip_frame.clone());
        while self.recent_frames.len() > self.pre_roll_frames.max(1) {
            self.recent_frames.pop_front();
        }

        let should_start_clip =
            self.active_clip.is_none() && !analysis.warming_up && analysis.motion_detected;

        if should_start_clip {
            let seeded_frames = self.recent_frames.iter().cloned().collect();
            self.active_clip = Some(ActiveClip {
                started_at_frame: frame_number,
                still_frames: 0,
                frames: seeded_frames,
            });
            println!(
                "Motion clip started at frame {frame_number}. Buffering {} pre-roll frame(s).",
                self.recent_frames.len()
            );
        }

        if let Some(active_clip) = &mut self.active_clip {
            if !should_start_clip {
                active_clip.frames.push(clip_frame);
            }

            if analysis.motion_detected {
                active_clip.still_frames = 0;
            } else {
                active_clip.still_frames += 1;
            }

            if active_clip.frames.len() >= self.max_clip_frames.max(1) {
                println!("Finishing clip because it reached the beginner-safe length limit.");
                return self.finish_active_clip();
            }

            let post_roll_reached = if self.post_roll_frames == 0 {
                !analysis.motion_detected
            } else {
                active_clip.still_frames >= self.post_roll_frames
            };

            if post_roll_reached {
                return self.finish_active_clip();
            }
        }

        Ok(None)
    }

    fn finish_pending_clip(&mut self) -> Result<Option<PathBuf>, Box<dyn Error>> {
        if self.active_clip.is_none() {
            return Ok(None);
        }

        self.finish_active_clip()
    }

    fn finish_active_clip(&mut self) -> Result<Option<PathBuf>, Box<dyn Error>> {
        let Some(active_clip) = self.active_clip.take() else {
            return Ok(None);
        };

        if active_clip.frames.is_empty() {
            return Ok(None);
        }

        let clip_path = self.next_clip_path(active_clip.started_at_frame);
        write_gif_clip(&clip_path, &active_clip.frames, self.effective_fps)?;
        println!(
            "Saved {} frame(s) to {}.",
            active_clip.frames.len(),
            clip_path.display()
        );
        self.next_clip_number += 1;

        Ok(Some(clip_path))
    }

    fn next_clip_path(&self, started_at_frame: usize) -> PathBuf {
        let timestamp_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        self.output_dir.join(format!(
            "clip_{:04}_frame_{started_at_frame}_{timestamp_millis}.gif",
            self.next_clip_number
        ))
    }
}

fn run_motion_detection(
    receiver: Receiver<CaptureMessage>,
    motion_config: MotionDetectorConfig,
    clip_config: ClipRecorderConfig,
    max_frames: Option<usize>,
    capture_info: CaptureInfo,
) -> Result<(), Box<dyn Error>> {
    let mut detector = MotionDetector::new(motion_config);
    let mut clip_recorder = ClipRecorder::new(
        clip_config,
        capture_info.width,
        capture_info.height,
        capture_info.frame_rate,
    )?;
    let started_at = Instant::now();
    let mut processed_frames = 0usize;

    println!(
        "Motion detection and clip saving are running on the main thread.{}",
        match max_frames {
            Some(limit) => format!(" It will stop after {limit} frames."),
            None => " Press Ctrl+C to stop.".to_string(),
        }
    );
    println!(
        "Settings: sample {}x{}, pixel threshold {}, motion threshold {} changed pixels.",
        motion_config.sample_width,
        motion_config.sample_height,
        motion_config.pixel_diff_threshold,
        motion_config.min_changed_pixels
    );
    println!(
        "Clip saving: GIF clips in {}, {}x{} at {} FPS, {} saved pre-roll frame(s), {} saved post-roll frame(s).",
        clip_recorder.output_dir().display(),
        clip_recorder.clip_width(),
        clip_recorder.clip_height(),
        clip_recorder.effective_fps(),
        clip_recorder.pre_roll_frames(),
        clip_recorder.post_roll_frames()
    );

    loop {
        if let Some(limit) = max_frames {
            if processed_frames >= limit {
                break;
            }
        }

        let message = match receiver.recv() {
            Ok(message) => message,
            Err(_) => break,
        };

        let captured_frame = match message {
            CaptureMessage::Started(_) => continue,
            CaptureMessage::Frame(captured_frame) => captured_frame,
            CaptureMessage::Error(message) => return Err(message.into()),
        };

        processed_frames += 1;
        let frame_number = captured_frame.frame_number;
        let rgb = captured_frame.rgb;
        let sampled_grayscale = sample_rgb_frame_to_grayscale(
            rgb.as_raw(),
            rgb.width() as usize,
            rgb.height() as usize,
            motion_config.sample_width,
            motion_config.sample_height,
        );
        let analysis = detector.analyze_frame(sampled_grayscale);

        let _ = clip_recorder.record_frame(&rgb, frame_number, &analysis)?;

        let should_print = analysis.warming_up
            || analysis.motion_started
            || analysis.motion_ended
            || frame_number % motion_config.report_every_n_frames == 0;

        if should_print {
            print_motion_status(frame_number, &analysis, started_at.elapsed());
        }
    }

    let _ = clip_recorder.finish_pending_clip()?;

    let elapsed_seconds = started_at.elapsed().as_secs_f64();
    let fps = processed_frames as f64 / elapsed_seconds.max(0.001);
    println!(
        "Processed {processed_frames} frames in {:.2} seconds ({fps:.1} FPS).",
        elapsed_seconds
    );

    Ok(())
}

fn print_motion_status(frame_number: usize, analysis: &MotionAnalysis, elapsed: Duration) {
    let seconds = elapsed.as_secs_f64();

    if analysis.warming_up {
        println!(
            "[{seconds:>6.2}s] Frame {frame_number:>4}: baseline captured, waiting for motion."
        );
        return;
    }

    let label = if analysis.motion_started {
        "MOTION STARTED"
    } else if analysis.motion_ended {
        "MOTION ENDED"
    } else if analysis.motion_detected {
        "MOTION"
    } else {
        "STILL"
    };

    println!(
        "[{seconds:>6.2}s] Frame {frame_number:>4}: {label:<14} changed {:>4}/{} sampled pixels",
        analysis.changed_pixels, analysis.total_pixels
    );
}

fn sample_rgb_frame_to_grayscale(
    rgb_pixels: &[u8],
    source_width: usize,
    source_height: usize,
    sample_width: usize,
    sample_height: usize,
) -> Vec<u8> {
    let mut sampled = Vec::with_capacity(sample_width * sample_height);

    for sample_y in 0..sample_height {
        let source_y = sample_y * source_height / sample_height;
        let row_start = source_y * source_width;

        for sample_x in 0..sample_width {
            let source_x = sample_x * source_width / sample_width;
            let rgb_index = (row_start + source_x) * 3;
            let red = u16::from(rgb_pixels[rgb_index]);
            let green = u16::from(rgb_pixels[rgb_index + 1]);
            let blue = u16::from(rgb_pixels[rgb_index + 2]);
            sampled.push(((red + green + blue) / 3) as u8);
        }
    }

    sampled
}

fn count_changed_pixels(previous_frame: &[u8], current_frame: &[u8], threshold: u8) -> usize {
    previous_frame
        .iter()
        .zip(current_frame.iter())
        .filter(|(previous, current)| previous.abs_diff(**current) >= threshold)
        .count()
}

fn duration_to_frame_count(duration: Duration, fps: u32) -> usize {
    if duration.is_zero() {
        return 0;
    }

    (duration.as_secs_f64() * f64::from(fps.max(1))).ceil() as usize
}

fn resize_for_clip(rgb_frame: &RgbImage, clip_width: u32, clip_height: u32) -> RgbaImage {
    let resized = image::imageops::resize(rgb_frame, clip_width, clip_height, FilterType::Triangle);
    DynamicImage::ImageRgb8(resized).into_rgba8()
}

fn write_gif_clip(
    output_path: &Path,
    frames: &[ClipFrame],
    effective_fps: u32,
) -> Result<(), Box<dyn Error>> {
    let file = File::create(output_path)?;
    let writer = BufWriter::new(file);
    let mut encoder = GifEncoder::new(writer);
    encoder.set_repeat(Repeat::Infinite)?;
    let delay = Delay::from_numer_denom_ms(1000, effective_fps.max(1));

    for clip_frame in frames {
        let gif_frame = Frame::from_parts(clip_frame.image.clone(), 0, 0, delay);
        encoder.encode_frame(gif_frame)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        MotionDetector, MotionDetectorConfig, count_changed_pixels, duration_to_frame_count,
        sample_rgb_frame_to_grayscale,
    };
    use std::time::Duration;

    #[test]
    fn rgb_sampling_picks_evenly_spaced_pixels() {
        let source = vec![
            0, 0, 0, 1, 1, 1, 2, 2, 2, 3, 3, 3, //
            4, 4, 4, 5, 5, 5, 6, 6, 6, 7, 7, 7, //
            8, 8, 8, 9, 9, 9, 10, 10, 10, 11, 11, 11, //
            12, 12, 12, 13, 13, 13, 14, 14, 14, 15, 15, 15,
        ];

        let sampled = sample_rgb_frame_to_grayscale(&source, 4, 4, 2, 2);
        assert_eq!(sampled, vec![0, 2, 8, 10]);
    }

    #[test]
    fn changed_pixels_respects_threshold() {
        let previous = vec![10, 10, 10, 10];
        let current = vec![10, 20, 40, 50];

        assert_eq!(count_changed_pixels(&previous, &current, 15), 2);
    }

    #[test]
    fn half_second_roll_matches_four_frames_at_eight_fps() {
        assert_eq!(duration_to_frame_count(Duration::from_millis(500), 8), 4);
    }

    #[test]
    fn detector_reports_motion_start_and_end() {
        let config = MotionDetectorConfig {
            sample_width: 2,
            sample_height: 2,
            pixel_diff_threshold: 10,
            min_changed_pixels: 2,
            report_every_n_frames: 1,
        };
        let mut detector = MotionDetector::new(config);

        let first = detector.analyze_frame(vec![10, 10, 10, 10]);
        assert!(first.warming_up);

        let second = detector.analyze_frame(vec![10, 10, 40, 40]);
        assert!(second.motion_started);
        assert!(second.motion_detected);

        let third = detector.analyze_frame(vec![10, 10, 10, 10]);
        assert!(!third.motion_ended);
        assert!(third.motion_detected);

        let fourth = detector.analyze_frame(vec![10, 10, 10, 10]);
        assert!(fourth.motion_ended);
        assert!(!fourth.motion_detected);
    }
}
