# Motion Detector

Rust-based motion detector for macOS webcams built with a beginner-friendly multithreaded pipeline. The app captures frames, detects motion from grayscale frame differences, saves motion-triggered clips, and sends real-time desktop alerts.

![Motion Detector Screenshot](docs/motion-detector-screenshot.png)

## About

This project is a technical Rust systems project focused on concurrency, message passing, and real-world webcam processing.

- Built in Rust with Cargo
- Designed for macOS webcam input using AVFoundation through `nokhwa`
- Uses a multithreaded producer-consumer pipeline
- Detects motion by comparing sampled grayscale frames
- Saves motion clips as GIFs with 0.5-second pre-roll and post-roll
- Sends desktop notifications when motion starts and ends

## Key Features

- Dedicated capture thread for webcam frame acquisition
- Main-thread motion detection loop for sampled grayscale analysis
- Dedicated clip-saver thread for motion-triggered GIF creation
- Dedicated alert thread for desktop notifications
- Bounded channels with `sync_channel` to keep the pipeline simple and safe
- Configurable camera index and optional frame limit for testing

## Here's How This App Can Help You!

- Monitor a room, desk, doorway, or workspace without recording nonstop video
- Capture only the moments that matter, which makes review much faster
- Receive immediate desktop alerts when motion begins or ends
- Learn beginner-friendly Rust concurrency through a practical project
- Use this project as a base for future upgrades such as email, webhook, or cloud alerts

## How It Works

The app currently uses four coordinated execution paths:

1. The capture thread opens the webcam and sends frames through a bounded channel.
2. The main thread receives frames and checks for motion using sampled grayscale pixel differences.
3. The clip-saver thread buffers frames and writes motion-triggered GIF clips.
4. The alert thread sends desktop notifications when motion starts or ends.

## Tech Stack

- Rust
- Cargo
- `std::thread`
- `std::sync::mpsc::sync_channel`
- `nokhwa`
- `image`
- AppleScript / `osascript` for macOS notifications

## Project Output

- Motion clips are saved in the `clips/` directory
- Saved clips are currently written as GIF files
- Desktop notifications appear when motion starts and ends
- Console logs show frame-by-frame motion status and thread activity

## Running The Program

### Prerequisites

- macOS
- Rust and Cargo installed
- Webcam access enabled for the app or terminal running the program
- Notification permission enabled if you want desktop alerts to appear

### Build

```bash
cargo build
```

### Run With Default Camera

```bash
cargo run -- 0
```

This uses camera index `0`.

### Run For A Short Test Session

```bash
cargo run -- 0 300
```

In this command:

- The first argument is the camera index
- The second argument is the optional maximum number of frames to process

### Expected Behavior

When the program starts, it should:

- Open the selected webcam
- Start the capture, motion detection, clip saving, and alert workflow
- Print motion activity to the terminal
- Save clips into `clips/`
- Show desktop notifications for motion start and end events

## Troubleshooting

### Camera Permission

If macOS blocks webcam access:

1. Open `System Settings`
2. Go to `Privacy & Security`
3. Open `Camera`
4. Enable access for the terminal or app running the program

### Notifications Not Appearing

If alerts do not appear:

1. Check macOS notification settings for the terminal or app running the project
2. Re-run the program after granting notification permission

## Why This Project Matters

This app is more than a webcam utility. It is also a clear example of how to build a real Rust concurrency project with threads, bounded channels, ownership-safe data flow, and practical output that users can see immediately.
