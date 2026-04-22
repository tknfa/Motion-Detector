use std::{
    error::Error,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use nokhwa::{
    Camera, native_api_backend, nokhwa_check, nokhwa_initialize,
    pixel_format::{LumaFormat, RgbFormat},
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
    println!("Opening camera index {camera_index}.");

    let requested =
        RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestFrameRate);
    let mut camera = Camera::with_backend(CameraIndex::Index(camera_index), requested, backend)?;

    let resolution = camera.resolution();
    println!(
        "Resolved camera format: {}x{} at {} FPS.",
        resolution.width(),
        resolution.height(),
        camera.frame_rate()
    );

    camera.open_stream()?;
    run_motion_detection(
        &mut camera,
        MotionDetectorConfig::default(),
        args.max_frames,
    )?;
    camera.stop_stream()?;

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

    fn analyze_frame(
        &mut self,
        grayscale_pixels: &[u8],
        source_width: usize,
        source_height: usize,
    ) -> MotionAnalysis {
        let sampled = sample_grayscale_frame(
            grayscale_pixels,
            source_width,
            source_height,
            self.config.sample_width,
            self.config.sample_height,
        );

        let total_pixels = sampled.len();
        let Some(previous_sample) = self.previous_sample.replace(sampled.clone()) else {
            return MotionAnalysis {
                changed_pixels: 0,
                total_pixels,
                motion_detected: false,
                motion_started: false,
                motion_ended: false,
                warming_up: true,
            };
        };

        let changed_pixels =
            count_changed_pixels(&previous_sample, &sampled, self.config.pixel_diff_threshold);
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

fn run_motion_detection(
    camera: &mut Camera,
    config: MotionDetectorConfig,
    max_frames: Option<usize>,
) -> Result<(), Box<dyn Error>> {
    let mut detector = MotionDetector::new(config);
    let started_at = Instant::now();
    let mut frame_number = 0usize;

    println!(
        "Single-thread motion detection is running.{}",
        match max_frames {
            Some(limit) => format!(" It will stop after {limit} frames."),
            None => " Press Ctrl+C to stop.".to_string(),
        }
    );
    println!(
        "Settings: sample {}x{}, pixel threshold {}, motion threshold {} changed pixels.",
        config.sample_width,
        config.sample_height,
        config.pixel_diff_threshold,
        config.min_changed_pixels
    );

    loop {
        if let Some(limit) = max_frames {
            if frame_number >= limit {
                break;
            }
        }

        frame_number += 1;
        let frame = camera.frame()?;
        let grayscale = frame.decode_image::<LumaFormat>()?;
        let analysis = detector.analyze_frame(
            grayscale.as_raw(),
            grayscale.width() as usize,
            grayscale.height() as usize,
        );

        let should_print = analysis.warming_up
            || analysis.motion_started
            || analysis.motion_ended
            || frame_number % config.report_every_n_frames == 0;

        if should_print {
            print_motion_status(frame_number, &analysis, started_at.elapsed());
        }
    }

    let elapsed_seconds = started_at.elapsed().as_secs_f64();
    let fps = frame_number as f64 / elapsed_seconds.max(0.001);
    println!(
        "Processed {frame_number} frames in {:.2} seconds ({fps:.1} FPS).",
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

fn sample_grayscale_frame(
    grayscale_pixels: &[u8],
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
            sampled.push(grayscale_pixels[row_start + source_x]);
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

#[cfg(test)]
mod tests {
    use super::{
        MotionDetector, MotionDetectorConfig, count_changed_pixels, sample_grayscale_frame,
    };

    #[test]
    fn sampling_picks_evenly_spaced_pixels() {
        let source = vec![
            0, 1, 2, 3, //
            4, 5, 6, 7, //
            8, 9, 10, 11, //
            12, 13, 14, 15,
        ];

        let sampled = sample_grayscale_frame(&source, 4, 4, 2, 2);
        assert_eq!(sampled, vec![0, 2, 8, 10]);
    }

    #[test]
    fn changed_pixels_respects_threshold() {
        let previous = vec![10, 10, 10, 10];
        let current = vec![10, 20, 40, 50];

        assert_eq!(count_changed_pixels(&previous, &current, 15), 2);
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

        let first = detector.analyze_frame(&[10, 10, 10, 10], 2, 2);
        assert!(first.warming_up);

        let second = detector.analyze_frame(&[10, 10, 40, 40], 2, 2);
        assert!(second.motion_started);
        assert!(second.motion_detected);

        let third = detector.analyze_frame(&[10, 10, 10, 10], 2, 2);
        assert!(!third.motion_ended);
        assert!(third.motion_detected);

        let fourth = detector.analyze_frame(&[10, 10, 10, 10], 2, 2);
        assert!(fourth.motion_ended);
        assert!(!fourth.motion_detected);
    }
}
