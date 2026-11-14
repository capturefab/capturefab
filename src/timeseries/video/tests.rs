//! Independent index literals exercise caller guards; encoder proof is separate.
use super::*;
use crate::types::classify;
use std::{fs, path::Path};

const FIRST: &[u8] = b"{\"frame_index\":0,\"video_time_ns\":0,\"frame_id\":17,\"received_monotonic_ns\":100,\"device_ts_ns\":700,\"width\":3,\"height\":2,\"pixel_format\":17301505}\n";
const SECOND: &[u8] = b"{\"frame_index\":1,\"video_time_ns\":100000000,\"frame_id\":18,\"received_monotonic_ns\":120,\"width\":3,\"height\":2,\"pixel_format\":17301505}\n";

fn video(folder: &Path) -> Video {
    let mut cameras = super::super::camera_session::Cameras::default();
    let session = cameras.get("sim:0", &[], None).unwrap();
    Video {
        config: serde_json::from_value(
            json!({"camera":"sim:0","output":folder.join("clip.mkv"),"codec":"ffv1","fps":10}),
        )
        .unwrap(),
        index: folder.join("frames.jsonl").to_str().unwrap().into(),
        session,
        job: None,
        forwarding_id: None,
        forwarding: None,
        startup_failure: None,
        reader: None,
        index_failure: None,
        pending: Vec::new(),
        frames: 0,
        observed: None,
        acquisition_end: Some(120),
        ended: None,
        finished: false,
        summary_sent: false,
    }
}

// Establish only the existing row-reader state. This does not pretend to be
// a worker ownership receipt or qualify an encoder/file finalization.
fn indexed(video: &mut Video, contents: &[u8]) {
    fs::write(&video.index, contents).unwrap();
    video.reader = Some(BufReader::new(File::open(&video.index).unwrap()));
    video.job = Some("direct-1".into());
}

fn ended(written: u64, finalized: bool) -> Value {
    json!({"job":{"id":"direct-1","state":"cancelled"},"detail":{
        "written":written,"finalized":finalized,"codec":"ffv1","encoder":"ffv1",
        "accepted":written,"dropped":0,"recording":{"bytes":321}}})
}

#[test]
fn no_observation_never_opens_or_drains_a_well_formed_foreign_index() {
    let folder = tempfile::tempdir().unwrap();
    let mut video = video(folder.path());
    fs::write(&video.index, FIRST).unwrap();
    video.startup_failure = Some(Arc::new(Categorized::error(
        ErrorKind::Timeout,
        "original startup cutoff",
    )));
    assert!(!video.attach_index().unwrap());
    assert_eq!(video.reader.as_ref().map(|_| ()), None);
    assert_eq!(
        classify(&video.read(Duration::ZERO).unwrap_err()).kind,
        ErrorKind::Timeout
    );
    video.finish(Duration::ZERO).unwrap();
    assert!(video.drain().unwrap().is_none());
    assert_eq!(fs::read(&video.index).unwrap(), FIRST);
    assert_eq!(video.job, None);
    assert_eq!(video.frames, 0);
}

#[test]
fn established_index_prefix_survives_startup_error_and_uses_only_actual_outcome() {
    let folder = tempfile::tempdir().unwrap();
    let mut video = video(folder.path());
    indexed(&mut video, &[FIRST, SECOND].concat());
    video.startup_failure = Some(Arc::new(Categorized::error(
        ErrorKind::Timeout,
        "first startup reply was lost",
    )));
    video.ended = Some(ended(2, true));
    video.finish(Duration::ZERO).unwrap();
    let first = video.drain().unwrap().unwrap();
    assert_eq!(first.signal, "frame");
    assert_eq!(first.value["frame_index"], 0);
    assert_eq!(first.value["frame_id"], 17);
    assert_eq!(first.value["device_ts_ns"], 700);
    assert_eq!(first.value["pixel_format"], "Mono8");
    assert_eq!(video.observed_ns(), Some(100));
    let second = video.drain().unwrap().unwrap();
    assert_eq!(second.value["frame_index"], 1);
    assert_eq!(second.value["frame_id"], 18);
    assert_eq!(video.observed_ns(), Some(120));
    let summary = video.drain().unwrap().unwrap();
    assert_eq!(summary.signal, "recording");
    assert_eq!(summary.value["frames"], 2);
    assert_eq!(summary.value["finalized"], true);
    assert_eq!(video.observed_ns(), None);
    assert!(video.drain().unwrap().is_none());
    let error = video.read(Duration::ZERO).unwrap_err();
    assert_eq!(classify(&error).kind, ErrorKind::Timeout);
    assert!(format!("{error:#}").contains("first startup reply was lost"));
}

#[test]
fn unfinished_final_row_retains_only_the_prior_complete_prefix() {
    let folder = tempfile::tempdir().unwrap();
    let mut video = video(folder.path());
    indexed(&mut video, &[FIRST, b"{\"frame_index\":1"].concat());
    video.finished = true;
    video.ended = Some(ended(2, true));
    assert_eq!(video.drain().unwrap().unwrap().value["frame_id"], 17);
    assert_eq!(
        classify(&video.drain().unwrap_err()).kind,
        ErrorKind::Incomplete
    );
    assert_eq!(video.frames, 1);
    assert!(!video.summary_sent);
}

#[test]
fn failed_read_never_resumes_at_a_later_plausible_row_during_final_drain() {
    let folder = tempfile::tempdir().unwrap();
    let mut video = video(folder.path());
    let mut invalid: Value = serde_json::from_slice(SECOND).unwrap();
    invalid["received_monotonic_ns"] = json!(99);
    let mut contents = FIRST.to_vec();
    contents.extend(serde_json::to_vec(&invalid).unwrap());
    contents.push(b'\n');
    contents.extend_from_slice(SECOND);
    indexed(&mut video, &contents);
    assert_eq!(video.next_frame().unwrap().unwrap().value["frame_id"], 17);
    assert_eq!(
        classify(&video.next_frame().unwrap_err()).kind,
        ErrorKind::Incomplete
    );
    video.ended = Some(ended(2, true));
    video.finish(Duration::ZERO).unwrap();
    let error = video.drain().unwrap_err();
    assert_eq!(classify(&error).kind, ErrorKind::Incomplete);
    assert!(format!("{error:#}").contains("invalid sequence or receive time"));
    assert_eq!(video.frames, 1);
    assert!(!video.summary_sent);
}

#[test]
fn sequence_receive_time_cutoff_and_line_cap_refuse_invalid_suffixes() {
    for patch in [
        json!({"frame_index":2}),
        json!({"received_monotonic_ns":99}),
        json!({"received_monotonic_ns":121}),
    ] {
        let folder = tempfile::tempdir().unwrap();
        let mut video = video(folder.path());
        let mut row: Value = serde_json::from_slice(SECOND).unwrap();
        for (key, value) in patch.as_object().unwrap() {
            row[key].clone_from(value);
        }
        let mut contents = FIRST.to_vec();
        contents.extend(serde_json::to_vec(&row).unwrap());
        contents.push(b'\n');
        indexed(&mut video, &contents);
        video.finished = true;
        assert_eq!(video.drain().unwrap().unwrap().value["frame_id"], 17);
        assert_eq!(
            classify(&video.drain().unwrap_err()).kind,
            ErrorKind::Incomplete
        );
        assert_eq!(video.frames, 1);
    }
    for (length, newline) in [
        (MAX_INDEX_LINE, true),
        (MAX_INDEX_LINE + 1, true),
        (MAX_INDEX_LINE + 1, false),
    ] {
        let folder = tempfile::tempdir().unwrap();
        let mut video = video(folder.path());
        let mut line = FIRST[..FIRST.len() - 1].to_vec();
        line.resize(length, b' ');
        if newline {
            *line.last_mut().unwrap() = b'\n';
        }
        indexed(&mut video, &line);
        video.finished = true;
        if length == MAX_INDEX_LINE {
            assert_eq!(video.drain().unwrap().unwrap().value["frame_id"], 17);
            assert_eq!(video.frames, 1);
        } else {
            assert_eq!(
                classify(&video.drain().unwrap_err()).kind,
                ErrorKind::Incomplete
            );
            assert_eq!(video.frames, 0);
        }
    }
}

#[test]
fn final_summary_requires_real_matching_count_and_never_invents_finalization() {
    let folder = tempfile::tempdir().unwrap();
    let mut video = video(folder.path());
    indexed(&mut video, FIRST);
    video.finished = true;
    video.ended = Some(ended(2, true));
    video.drain().unwrap().unwrap();
    assert_eq!(
        classify(&video.drain().unwrap_err()).kind,
        ErrorKind::Incomplete
    );

    let mut video = self::video(folder.path());
    indexed(&mut video, FIRST);
    video.finished = true;
    video.ended = Some(ended(1, false));
    video.drain().unwrap().unwrap();
    assert!(video.drain().unwrap().is_none());
}
