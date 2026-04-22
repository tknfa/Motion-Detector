use std::{
    error::Error,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use nokhwa::{
    Camera, native_api_backend, nokhwa_check, nokhwa_initialize,
    pixel_format::RgbFormat,
    query,
    utils::{CameraIndex, RequestedFormat, RequestedFormatType},
};

fn main() -> Result<(), Box<dyn Error>> {
    wait_for_camera_permission()?;

    let backend =
        native_api_backend().ok_or("No native camera backend is available on this OS.")?;
    let cameras = query(backend)?;

    if cameras.is_empty() {
        return Err("No webcams were found.".into());
    }

    let camera_index = read_camera_index();
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
    capture_test_frames(&mut camera, 120)?;
    camera.stop_stream()?;

    println!("Success: webcam capture is working.");
    Ok(())
}

fn read_camera_index() -> u32 {
    std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
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

fn capture_test_frames(
    camera: &mut Camera,
    frames_to_capture: usize,
) -> Result<(), Box<dyn Error>> {
    let started_at = Instant::now();

    for frame_number in 1..=frames_to_capture {
        let frame = camera.frame()?;

        if frame_number == 1 || frame_number % 30 == 0 || frame_number == frames_to_capture {
            let resolution = frame.resolution();
            println!(
                "Frame {:>3}: {}x{}, {} bytes",
                frame_number,
                resolution.width(),
                resolution.height(),
                frame.buffer().len()
            );
        }
    }

    let elapsed_seconds = started_at.elapsed().as_secs_f64();
    let fps = frames_to_capture as f64 / elapsed_seconds.max(0.001);
    println!(
        "Captured {frames_to_capture} frames in {:.2} seconds ({fps:.1} FPS).",
        elapsed_seconds
    );

    Ok(())
}
