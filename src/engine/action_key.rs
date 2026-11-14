//! The sole admitted key write, with absolute expiry and no transaction history.
use super::{AutoController, Camera, WorkerHandle, camera_mut};
use crate::{
    session::action_key,
    types::{Categorized, ErrorKind},
};
use anyhow::Result;
use serde_json::Value;

pub(super) fn check(h: &WorkerHandle, deadline_ns: u64) -> Result<()> {
    if h.cancelled() {
        return Err(action_key::no_write(Categorized::error(
            ErrorKind::Interrupted,
            "action key cancelled before writing",
        )));
    }
    action_key::remaining(deadline_ns).map(drop)
}

pub(super) fn write(
    h: &WorkerHandle,
    camera: &mut Option<Camera>,
    auto: &mut Option<AutoController>,
    key: u32,
    deadline_ns: u64,
) -> Result<Value> {
    check(h, deadline_ns)?;
    let deadline = action_key::instant(deadline_ns)?;
    let c = camera_mut(camera).map_err(action_key::no_write)?;
    if let Some(error) = c.terminal_error() {
        return Err(action_key::no_write(error));
    }
    if let Some(auto) = auto.as_mut() {
        auto.invalidate();
    }
    check(h, deadline_ns)?;
    let result = c.write_action_device_key(key, deadline)?;
    if let Some(error) = c.terminal_error() {
        return Err(action_key::dispatched(&error, true));
    }
    action_key::remaining(deadline_ns).map_err(|error| action_key::dispatched(&error, true))?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use crate::{
        engine::*,
        transport::gige::action_workflow_tests::{Harness, PIXELS},
        types::ErrorKind,
    };
    use serde_json::json;

    #[test]
    fn owned_native_action_ack_is_separate_from_real_scheduled_pixels_and_saver_completion() {
        saved_action(false);
    }

    #[test]
    fn a_scheduled_action_ack_precedes_clock_release_and_real_raw_png_saver_completion() {
        saved_action(true);
    }

    fn saved_action(scheduled: bool) {
        for format in ["raw", "png"] {
            let peer = Harness::new();
            let handle = WorkerHandle::idle();
            let mut camera = Some(peer.open());
            let mut forwarder = None;
            let mut auto = None;
            let mut jobs = Vec::new();
            let mut next_job = 1;
            let mut stream = JobStream::default();
            let dir = tempfile::tempdir().unwrap();
            let output = dir.path().join(format!("action.{format}"));
            {
                let mut run = |command| {
                    process(
                        &handle,
                        CommandState {
                            camera: &mut camera,
                            forwarder: &mut forwarder,
                            auto: &mut auto,
                            jobs: &mut jobs,
                            next_job: &mut next_job,
                            scheduled_stream: &mut stream,
                        },
                        command,
                    )
                };
                // Admission's absolute host expiry, not a duration restarted by
                // this engine dispatch. The actual write is native GVCP/WO XML.
                let deadline =
                    crate::session::action_key::deadline_after(Duration::from_secs(2)).unwrap();
                let result = run(WorkerCommand::WriteActionDeviceKeyUntil {
                    key: 0x1234_5678,
                    deadline_ns: deadline,
                })
                .unwrap();
                assert_eq!(result["completion"], "native_transport_ack");
                assert_eq!(peer.snapshot().key_writes, 1);
                assert_eq!(result["readback"], "unavailable_write_only");
                assert!(result.get("key").is_none());
                let values = serde_json::from_value(json!([
                    {"name":"ActionGroupKey","value":"2309737967"},
                    {"name":"ActionGroupMask","value":"2"},
                    {"name":"TriggerSource","value":"Action1"},
                    {"name":"TriggerMode","value":"On"}
                ]))
                .unwrap();
                run(SessionCommand::SetValues {
                    values,
                    dry_run: false,
                }
                .into())
                .unwrap();
                let scheduled=SessionCommand::from_json_slice(json!({"op":"schedule","output":output,"count":1,
                "timeout_ms":3000,"format":format,"first_at_ms":0,"interval_ms":0,"warmup_ms":0,"fresh":0,"min_free":0}).to_string().as_bytes()).unwrap();
                run(scheduled.into()).unwrap();
            }
            tick_jobs(&handle, &mut camera, &mut jobs, &mut stream, false, false);
            assert!(peer.snapshot().acquisition_started);
            assert_eq!(jobs[0].info.captured, 0);
            assert!(!output.exists());
            if scheduled {
                let ack = peer
                    .issue_scheduled(0x1234_5678, 0x89ab_cdef, 2, 2048)
                    .unwrap();
                assert_eq!(ack["action_time_ticks"], 2048);
                assert_eq!(ack["execution"], "not_verified");
                assert_eq!(ack["frame_arrival"], "not_verified");
                let queued = peer.snapshot();
                assert_eq!(queued.pending_action, Some(2048));
                assert_eq!(queued.device_ticks, 1000);
                assert_eq!(queued.frame_count, 0);
                // Independent literal: scheduled+ACK flag, body20, request3579,
                // device/group/mask, then big-endian absolute fixture ticks.
                assert_eq!(
                    queued.actions,
                    [vec![
                        0x42, 0x81, 0x01, 0x00, 0x00, 0x14, 0x35, 0x79, 0x12, 0x34, 0x56, 0x78,
                        0x89, 0xab, 0xcd, 0xef, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x08, 0x00,
                    ]]
                );
                camera
                    .as_mut()
                    .unwrap()
                    .next_frame(Duration::from_millis(30))
                    .unwrap_err();
                peer.advance_clock(2047);
                camera
                    .as_mut()
                    .unwrap()
                    .next_frame(Duration::from_millis(30))
                    .unwrap_err();
                assert_eq!(peer.snapshot().pending_action, Some(2048));
                assert_eq!(peer.snapshot().frame_count, 0);
                assert_eq!(jobs[0].info.captured, 0);
                assert!(!output.exists());
                peer.advance_clock(2048);
                assert_eq!(peer.snapshot().pending_action, None);
            } else {
                peer.issue(0x1234_5679, 0x89ab_cdef, 2, false).unwrap();
                peer.issue(0x1234_5678, 0x89ab_cdef, 4, false).unwrap();
                camera
                    .as_mut()
                    .unwrap()
                    .next_frame(Duration::from_millis(30))
                    .unwrap_err();
                assert_eq!(peer.snapshot().frame_count, 0);
                peer.allow_frames(false);
                let ack = peer.issue(0x1234_5678, 0x89ab_cdef, 2, true).unwrap();
                assert_eq!(ack["frame_arrival"], "not_verified");
                camera
                    .as_mut()
                    .unwrap()
                    .next_frame(Duration::from_millis(30))
                    .unwrap_err();
                assert_eq!(jobs[0].info.captured, 0);
                assert!(!output.exists());
                peer.allow_frames(true);
                peer.issue(0x1234_5678, 0x89ab_cdef, 2, true).unwrap();
            }
            let c = camera.as_mut().unwrap();
            let frame = c.next_frame(Duration::from_secs(1)).unwrap();
            assert_eq!(frame.id, 42);
            assert_eq!(frame.data.as_ref(), &PIXELS);
            assert_eq!(frame.device_ts_ns, Some(1_234_000_000));
            let mut saver = saver::Saver::new(handle.clone());
            let saves = shoot(&handle, c, &mut jobs, true, true, frame.wall_ms);
            assert_eq!(saves.len(), 1);
            saver.offer(saver::Shot {
                wall_ms: frame.wall_ms,
                frame,
                acquisition: acquisition(c),
                saves,
            });
            let until = Instant::now() + Duration::from_secs(5);
            while jobs[0].saving > 0 && Instant::now() < until {
                saved(&handle, &mut jobs, &saver);
                std::thread::sleep(Duration::from_millis(2));
            }
            assert_eq!(jobs[0].info.captured, 1, "{:?}", jobs[0].info.error);
            let files = handle.take_saved();
            assert_eq!(files.len(), 1);
            assert_eq!(
                files[0].1.manifest_line("fixture", &json!({}))["frame_id"],
                42
            );
            let data = std::fs::read(&output).unwrap();
            if format == "raw" {
                assert_eq!(data, PIXELS);
            } else {
                let mut reader = png::Decoder::new(std::io::Cursor::new(data))
                    .read_info()
                    .unwrap();
                let mut pixels = vec![0; reader.output_buffer_size()];
                let decoded = reader.next_frame(&mut pixels).unwrap();
                assert_eq!(&pixels[..decoded.buffer_size()], &PIXELS);
            }
            let snapshot = peer.snapshot();
            assert_eq!(snapshot.actions.len(), if scheduled { 1 } else { 4 });
            assert_eq!(snapshot.key_reads, 0);
            assert_eq!(snapshot.key_writes, 1);
            assert_eq!(snapshot.frame_count, 1);
            assert_eq!(snapshot.claims, 1);
            assert_eq!(snapshot.starts, 1);
            assert_eq!(snapshot.events.first(), Some(&"acquisition_start"));
            if scheduled {
                assert_eq!(
                    snapshot.events,
                    [
                        "acquisition_start",
                        "action",
                        "scheduled_queued",
                        "clock_advance",
                        "clock_advance",
                        "frame",
                    ]
                );
            }
            drop(saver.join());
            camera.as_mut().unwrap().stop().unwrap();
            drop(camera.take());
            let until = Instant::now() + Duration::from_secs(1);
            while peer.snapshot().releases == 0 && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(2));
            }
            assert_eq!(peer.snapshot().releases, 1);
            handle.shutdown();
        }
    }

    #[test]
    fn private_worker_queue_expiry_and_known_cancel_never_write_the_live_camera_key() {
        let peer = Harness::new();
        let handle = WorkerHandle::idle();
        let mut camera = Some(peer.open());
        for cancel in [true, false] {
            let deadline =
                crate::session::action_key::deadline_after(Duration::from_millis(500)).unwrap();
            let command = WorkerCommand::WriteActionDeviceKeyUntil {
                key: 7,
                deadline_ns: deadline,
            };
            let (tx, rx) = mpsc::channel();
            handle
                .queue_request(
                    Some(91),
                    command,
                    Box::new(move |result| {
                        let _ = tx.send(result);
                    }),
                )
                .map_err(|(error, _)| error)
                .unwrap();
            if !cancel {
                std::thread::sleep(Duration::from_millis(550));
            }
            let envelope = handle.next_request(Duration::ZERO).unwrap();
            if cancel {
                handle.cancel(91);
            }
            let result = process(
                &handle,
                CommandState {
                    camera: &mut camera,
                    forwarder: &mut None,
                    auto: &mut None,
                    jobs: &mut Vec::new(),
                    next_job: &mut 1,
                    scheduled_stream: &mut JobStream::default(),
                },
                envelope.command,
            );
            (envelope.reply)(result);
            let error = rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap_err();
            assert_eq!(
                crate::types::classify(&error).kind,
                if cancel {
                    ErrorKind::Interrupted
                } else {
                    ErrorKind::Timeout
                }
            );
            assert_eq!(
                crate::types::classify(&error).details.unwrap()["write_may_have_applied"],
                false
            );
            assert_eq!(peer.snapshot().key_writes, 0);
            *handle.running.lock().unwrap() = None;
        }
        // Even a syntactically valid public op cannot be sent directly to a
        // worker and acquire a fresh timeout in place of coordinator admission.
        let error = handle
            .submit(SessionCommand::WriteActionDeviceKey {
                key: 7,
                timeout_ms: 1000,
            })
            .map(drop)
            .unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Usage);
        assert_eq!(peer.snapshot().key_writes, 0);
        assert_eq!(peer.snapshot().key_reads, 0);
        drop(camera.take());
        handle.shutdown();
    }
}
