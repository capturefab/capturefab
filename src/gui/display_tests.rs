//! How the workbench sizes the frames it shows, and paces its scopes.
use super::*;
use crate::{session::CameraSnapshot, shared_memory::SharedRing, types::Frame};

/// A workbench showing the selected camera of a ring this test writes to,
/// converting on the CPU, out of efficiency mode whatever the host.
fn bench() -> (Workbench, SharedRing) {
    crate::efficiency::pin(false);
    let (handle, writer) = SessionHandle::fed(8 << 20);
    let mut bench = Workbench::new(handle, "test".into(), true, None);
    bench.gpu = GpuFrames::cpu_only();
    bench.snapshot.active_camera = Some("cam".into());
    bench.stage.set(Size::new(800.0, 600.0));
    (bench, writer)
}

fn converted(bench: &Workbench) -> Option<(u32, u32)> {
    bench.shown.as_ref().and_then(Shown::converted)
}

fn frame(id: u64) -> Frame {
    Frame {
        id,
        width: 2000,
        height: 1500,
        pixel_format: crate::types::MONO8,
        timestamp_ns: id,
        data: (0..3_000_000).map(|i| (i % 251) as u8).collect(),
    }
}

/// Sizes of the frames waiting for the GPU under `key`.
fn uploaded(gpu: &GpuFrames, key: &str) -> Option<(u32, u32)> {
    gpu.pending(key).map(|frame| (frame.width, frame.height))
}

/// Streaming cameras by `ids`, for the overview.
fn cameras<const N: usize>(ids: [&str; N]) -> Vec<CameraSnapshot> {
    ids.map(|id| {
        let mut camera = liveness::streaming_camera(0, 0, 0.0);
        camera.info.id = id.into();
        camera
    })
    .into()
}

#[test]
fn the_main_view_converts_what_the_stage_shows_and_every_pixel_at_one_to_one() {
    let (mut bench, mut writer) = bench();
    writer.write(&frame(1), false).unwrap();
    bench.update_frames();
    // Fitted at 0.4 in an 800 x 600 stage.
    assert_eq!(converted(&bench), Some((1000, 750)));
    assert_eq!(
        bench.shown.as_ref().unwrap().size(),
        Size::new(2000.0, 1500.0)
    );
    // Zooming converts the same, stopped, frame again at once.
    let _ = bench.update(Message::Actual);
    assert_eq!(converted(&bench), Some((2000, 1500)));
    let _ = bench.update(Message::Fit);
    assert_eq!(converted(&bench), Some((1000, 750)));
    // Half of fitted, 0.2: shrunk five times.
    let _ = bench.update(Message::Zoom(0.5));
    assert_eq!(converted(&bench), Some((400, 300)));
    let _ = bench.dispatch_action(Action::ZoomActual);
    assert_eq!(converted(&bench), Some((2000, 1500)));
    let _ = bench.dispatch_action(Action::ZoomFit);
    // A display at twice the scale needs twice the pixels; the next tick
    // shows it.
    bench.window_scales.insert(bench.main_window, 2.0);
    bench.update_frames();
    assert_eq!(converted(&bench), Some((2000, 1500)));
    // A larger stage while stopped, likewise.
    bench.window_scales.clear();
    bench.update_frames();
    assert_eq!(converted(&bench), Some((1000, 750)));
    bench.stage.set(Size::new(1600.0, 1200.0));
    bench.update_frames();
    assert_eq!(converted(&bench), Some((2000, 1500)));
    bench.handle.shutdown();
}

#[test]
fn efficiency_mode_keeps_1280_pixels_a_side_on_the_gpu_too_short_of_one_to_one() {
    let (mut bench, mut writer) = bench();
    let gpu = GpuFrames::limited(8192);
    bench.gpu = gpu.clone();
    // A 5 MP mosaic fitted on a display at twice the scale: 0.59 of it shows.
    bench.window_scales.insert(bench.main_window, 2.0);
    let mosaic = Frame {
        id: 1,
        width: 2448,
        height: 2048,
        pixel_format: 0x0108_0009,
        timestamp_ns: 1,
        data: vec![90; 2448 * 2048],
    };
    writer.write(&mosaic, false).unwrap();
    bench.update_frames();
    assert_eq!(uploaded(&gpu, MAIN_VIEW), Some((2448, 2048)));
    crate::efficiency::pin(true);
    bench.update_frames();
    assert_eq!(uploaded(&gpu, MAIN_VIEW), Some((1224, 1024)));
    assert_eq!(
        bench.shown.as_ref().unwrap().size(),
        Size::new(2448.0, 2048.0),
        "placed at its full size"
    );
    let _ = bench.update(Message::Actual);
    assert_eq!(
        uploaded(&gpu, MAIN_VIEW),
        Some((2448, 2048)),
        "1:1 stays exact"
    );
    // An overview tile at more than half size, likewise.
    bench.snapshot.cameras = cameras(["cam"]);
    bench.grid.set(Some(grid::Grid {
        columns: 1,
        tile: Size::new(800.0, 600.0),
        viewport: 600.0,
    }));
    bench.update_previews();
    assert_eq!(uploaded(&gpu, "cam"), Some((1224, 1024)));
    crate::efficiency::pin(false);
    bench.update_previews();
    assert_eq!(uploaded(&gpu, "cam"), Some((2448, 2048)));
    // Without a GPU it converts as much.
    crate::efficiency::pin(true);
    bench.gpu = GpuFrames::cpu_only();
    let _ = bench.update(Message::Fit);
    assert_eq!(converted(&bench), Some((1224, 1024)));
    bench.handle.shutdown();
}

#[test]
fn scopes_measure_a_stream_at_their_own_pace() {
    let (mut bench, mut writer) = bench();
    bench.exposure_open = true;
    bench.focus.open = true;
    let (mut histograms, mut scores) = (0, 0);
    // One second of frames at 30 fps.
    for id in 1..=30 {
        writer.write(&frame(id), false).unwrap();
        bench.now += Duration::from_micros(33_333);
        let before = bench.measured;
        bench.update_frames();
        assert_eq!(bench.frame_id, Some(id), "every frame is shown");
        histograms += usize::from(bench.measured.0 != before.0);
        scores += usize::from(bench.measured.1 != before.1);
    }
    assert!((5..=6).contains(&histograms), "{histograms} histograms");
    assert!((15..=16).contains(&scores), "{scores} focus scores");
    assert!(bench.exposure.is_some() && bench.focus.latest().is_some());
    // Another camera's first frame, as a switch leaves it, measures at once.
    bench.frame_id = None;
    writer.write(&frame(31), false).unwrap();
    bench.now += Duration::from_millis(1);
    bench.update_frames();
    let now = Measured {
        at: Some(bench.now),
        frame: Some(31),
    };
    assert_eq!(bench.measured, (now, now));
    bench.handle.shutdown();
}

#[test]
fn scopes_measure_the_frame_a_stream_stops_on() {
    let (mut bench, mut writer) = bench();
    bench.exposure_open = true;
    bench.focus.open = true;
    // The second frame comes too soon for either scope, and is the last.
    for id in 1..=2 {
        writer.write(&frame(id), false).unwrap();
        bench.now += Duration::from_micros(33_333);
        bench.update_frames();
    }
    assert_eq!(bench.measured.0.frame, Some(1));
    assert_eq!(bench.measured.1.frame, Some(1));
    // Ticks measure it once each scope is due, and once only.
    let mut changes = (0, 0);
    for _ in 0..12 {
        bench.now += Duration::from_micros(33_333);
        let before = bench.measured;
        bench.update_frames();
        changes.0 += usize::from(bench.measured.0 != before.0);
        changes.1 += usize::from(bench.measured.1 != before.1);
    }
    assert_eq!(changes, (1, 1));
    assert_eq!(bench.measured.0.frame, Some(2));
    assert_eq!(bench.measured.1.frame, Some(2));
    bench.handle.shutdown();
}

#[test]
fn dragging_the_focus_region_scores_only_focus_and_once_more_on_release() {
    let (mut bench, mut writer) = bench();
    bench.exposure_open = true;
    bench.focus.open = true;
    writer.write(&frame(1), false).unwrap();
    bench.update_frames();
    let histogram = bench.measured.0;
    assert!(histogram.at.is_some());
    let region = |x| iced::Rectangle::new(iced::Point::new(x, 0.25), Size::new(0.2, 0.2));
    let scored = |bench: &Workbench| bench.measured.1.frame == Some(1);
    // Messages read the clock, so "scored just now" is a time to come.
    let just = Instant::now() + Duration::from_secs(60);
    // Moves within a frame's time of the last score are not scored, and
    // leave that score on show.
    bench.measured.1.at = Some(just);
    let _ = bench.update(Message::FocusRegion(region(0.1)));
    assert_eq!(bench.measured.1.at, Some(just));
    assert!(!scored(&bench));
    assert!(
        bench.focus.latest().is_some(),
        "the last score stays on show"
    );
    bench.measured.1.at = None;
    let _ = bench.update(Message::FocusRegion(region(0.3)));
    assert!(scored(&bench) && bench.measured.1.at != Some(just));
    bench.measured.1.at = Some(just);
    let _ = bench.update(Message::FocusRegion(region(0.5)));
    assert!(!scored(&bench) && bench.focus.latest().is_some());
    // Held still, the region is scored on the first tick a frame's time on.
    let tick = just + FOCUS_EVERY;
    bench.now = tick;
    bench.update_frames();
    assert_eq!(bench.measured.1.at, Some(tick));
    assert!(scored(&bench));
    // Letting go scores where it landed, at once, unless scored there.
    let _ = bench.update(Message::FocusRegionEnd);
    assert_eq!(bench.measured.1.at, Some(tick));
    bench.measured.1.at = Some(just);
    let _ = bench.update(Message::FocusRegion(region(0.6)));
    bench.measured.1.at = Some(Instant::now());
    let _ = bench.update(Message::FocusRegionEnd);
    assert!(scored(&bench));
    assert_eq!(bench.measured.0, histogram, "the histogram never changed");
    // A click right after moves the region without a drag, and is scored.
    let _ = bench.update(Message::FocusRegionStart);
    let _ = bench.update(Message::FocusRegion(region(0.7)));
    assert!(scored(&bench));
    bench.handle.shutdown();
}

#[test]
fn tiles_scrolled_out_of_view_let_go_of_their_frames() {
    let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
    bench.snapshot.cameras = (0..6)
        .map(|i| {
            let mut camera = liveness::streaming_camera(0, 0, 0.0);
            camera.info.id = format!("tile{i}");
            camera
        })
        .collect();
    let gpu = GpuFrames::limited(8192);
    bench.gpu = gpu.clone();
    for camera in &bench.snapshot.cameras {
        let id = camera.info.id.clone();
        gpu.submit(&id, std::sync::Arc::new(Frame::default()));
        bench.previews.insert(
            id.clone(),
            CameraPreview {
                frame_id: Some(1),
                shown: Some(Shown::Gpu {
                    key: id,
                    size: Size::new(640.0, 480.0),
                }),
                ..CameraPreview::default()
            },
        );
    }
    // Two columns, three rows, two of them in view.
    bench.grid.set(Some(grid::Grid {
        columns: 2,
        tile: Size::new(400.0, 300.0),
        viewport: 600.0,
    }));
    bench.update_previews();
    for (i, camera) in bench.snapshot.cameras.iter().enumerate() {
        let id = &camera.info.id;
        let scrolled_out = i >= 4;
        assert_eq!(gpu.is_retired(id), scrolled_out, "{id}");
        assert_eq!(gpu.pending(id).is_none(), scrolled_out, "{id}");
        assert_eq!(bench.previews[id].frame_id.is_none(), scrolled_out, "{id}");
    }
    // Scrolling back shows them again with their newest frames.
    let _ = bench.update(Message::OverviewScrolled(300.0));
    assert_eq!(bench.grid_scroll, 300.0);
}
