//! Exercise timeout policy through the actual acquisition and finalization loop.
use super::{AcquisitionClock, Reading, Source, SourceConfig, run_source};
use crate::types::{Categorized, ErrorKind, classify};
use anyhow::Result;
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

struct Replies {
    successful: bool,
    idle_as_success: bool,
    finished: Arc<AtomicUsize>,
}
impl Source for Replies {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        if !self.successful {
            self.successful = true;
            return Ok(vec![Reading {
                signal: "retained".into(),
                value: json!(42),
                source_time: None,
            }]);
        }
        thread::sleep(timeout + Duration::from_millis(1));
        if self.idle_as_success {
            return Ok(Vec::new());
        }
        Err(Categorized::error(
            ErrorKind::Timeout,
            "next frame did not finish before the acquisition cutoff",
        ))
    }
    fn finish(&mut self, _timeout: Duration) -> Result<()> {
        self.finished.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

struct QuietGap(Replies);
impl Source for QuietGap {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        self.0.read(timeout)
    }
    fn end_timeout_is_idle(&self) -> bool {
        true
    }
    fn finish(&mut self, timeout: Duration) -> Result<()> {
        self.0.finish(timeout)
    }
}

#[test]
fn cutoff_fails_incomplete_operations_and_keeps_idle_and_completed_samples() {
    for (polled, quiet_gap, idle_as_success, should_fail) in [
        (true, false, false, true),
        (false, false, false, true),
        (false, false, true, false),
        (true, true, false, false),
    ] {
        let mut source = if polled {
            json!({"id":"device","protocol":"modbus_tcp","address":"127.0.0.1:1",
                "signals":[{"name":"retained","address":0}]})
        } else {
            json!({"id":"device","protocol":"tcp_jsonl","address":"127.0.0.1:1"})
        };
        source["every_ms"] = json!(1);
        let source: SourceConfig = serde_json::from_value(source).unwrap();
        assert_eq!(source.adapter.is_polled(), polled);
        let finished = Arc::new(AtomicUsize::new(0));
        let replies = Replies {
            successful: false,
            idle_as_success,
            finished: Arc::clone(&finished),
        };
        let adapter: Box<dyn Source> = if quiet_gap {
            Box::new(QuietGap(replies))
        } else {
            Box::new(replies)
        };
        let (sender, receiver) = mpsc::sync_channel(4);
        let start = Instant::now();
        let result = run_source(
            adapter,
            &source,
            &sender,
            &AtomicBool::new(false),
            AcquisitionClock {
                start,
                end: start + Duration::from_millis(100),
                host_origin: None,
            },
        );
        if should_fail {
            assert_eq!(classify(&result.unwrap_err()).kind, ErrorKind::Timeout);
        } else {
            result.unwrap();
        }
        assert_eq!(finished.load(Ordering::Relaxed), 1);
        drop(sender);
        let batches: Vec<_> = receiver.into_iter().collect();
        let [batch] = batches.as_slice() else {
            panic!("exactly the completed first poll must remain recoverable");
        };
        assert_eq!(batch.readings.len(), 1);
        assert_eq!(batch.readings[0].value, 42);
    }
}

struct CheckpointSource {
    deadline: Option<Instant>,
    acknowledged: Arc<AtomicUsize>,
}
impl Source for CheckpointSource {
    fn read(&mut self, _timeout: Duration) -> Result<Vec<Reading>> {
        Ok(Vec::new())
    }
    fn checkpoint_deadline(&self) -> Option<Instant> {
        self.deadline
    }
    fn checkpointed(&mut self) -> Result<()> {
        self.acknowledged.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}
fn observation(value: u64) -> Vec<Reading> {
    vec![Reading {
        signal: "result".into(),
        value: json!(value),
        source_time: None,
    }]
}
fn output<'a>(
    sender: &'a mpsc::SyncSender<super::Batch>,
    stop: &'a AtomicBool,
    start: Instant,
) -> super::SourceOutput<'a> {
    super::SourceOutput {
        source: "checkpointed",
        sender,
        stop,
        clock: AcquisitionClock {
            start,
            end: start + Duration::from_secs(2),
            host_origin: None,
        },
    }
}

#[test]
fn checkpoints_wait_for_the_matching_written_batch_and_keep_append_order() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("checkpoint.jsonl");
    let mut file = crate::storage::Recording::create(&path, None, None).unwrap();
    let acknowledged = Arc::new(AtomicUsize::new(0));
    let mut source = CheckpointSource {
        deadline: Some(Instant::now() + Duration::from_secs(2)),
        acknowledged: Arc::clone(&acknowledged),
    };
    let (send, receive) = mpsc::sync_channel(2);
    let stop = AtomicBool::new(false);
    let start = Instant::now();
    let output = output(&send, &stop, start);
    let mut samples = 0;
    thread::scope(|scope| {
        let worker = scope.spawn(|| {
            output.send(&mut source, observation(1), start, None)?;
            output.send(&mut source, observation(2), start, None)
        });
        let first = receive.recv_timeout(Duration::from_secs(1)).unwrap();
        let stale = first.checkpoint.as_ref().unwrap().clone();
        assert_eq!(acknowledged.load(Ordering::Relaxed), 0);
        super::write_batch(&mut file, first, &mut samples, chrono::Utc::now(), &stop).unwrap();
        let second = receive.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(acknowledged.load(Ordering::Relaxed), 1);
        stale.try_send(()).unwrap_err();
        assert_eq!(acknowledged.load(Ordering::Relaxed), 1);
        super::write_batch(&mut file, second, &mut samples, chrono::Utc::now(), &stop).unwrap();
        worker.join().unwrap().unwrap();
    });
    assert_eq!(acknowledged.load(Ordering::Relaxed), 2);
    assert_eq!(samples, 2);
    let rows: Vec<serde_json::Value> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let [first, second] = rows.as_slice() else {
        panic!("exactly the two checkpointed rows must be present");
    };
    assert_eq!(first["seq"], 1);
    assert_eq!(first["value"], 1);
    assert_eq!(second["seq"], 2);
    assert_eq!(second["value"], 2);
    file.finish().unwrap();
}

#[test]
fn pending_empty_batches_and_expired_or_cancelled_admission_never_acknowledge() {
    for (empty, cancelled, expired, kind) in [
        (true, false, false, ErrorKind::Incomplete),
        (false, false, true, ErrorKind::Timeout),
        (false, true, false, ErrorKind::Interrupted),
    ] {
        let acknowledged = Arc::new(AtomicUsize::new(0));
        let start = Instant::now();
        let mut source = CheckpointSource {
            deadline: Some(if expired {
                start
            } else {
                start + Duration::from_secs(1)
            }),
            acknowledged: Arc::clone(&acknowledged),
        };
        let (send, receive) = mpsc::sync_channel(1);
        let stop = AtomicBool::new(cancelled);
        let result = output(&send, &stop, start).send(
            &mut source,
            if empty { Vec::new() } else { observation(1) },
            start,
            None,
        );
        assert_eq!(classify(&result.unwrap_err()).kind, kind);
        assert!(matches!(receive.try_recv(), Err(mpsc::TryRecvError::Empty)));
        assert_eq!(acknowledged.load(Ordering::Relaxed), 0);
    }
    // A still-live operation budget cannot authorize a checkpoint after acquisition.
    let start = Instant::now();
    let acknowledged = Arc::new(AtomicUsize::new(0));
    let mut source = CheckpointSource {
        deadline: Some(start + Duration::from_secs(2)),
        acknowledged: Arc::clone(&acknowledged),
    };
    let (send, receive) = mpsc::sync_channel(1);
    let stop = AtomicBool::new(false);
    let mut output = output(&send, &stop, start);
    output.clock.end = start;
    let error = output
        .send(&mut source, observation(1), start, None)
        .unwrap_err();
    assert_eq!(classify(&error).kind, ErrorKind::Timeout);
    assert!(matches!(receive.try_recv(), Err(mpsc::TryRecvError::Empty)));
    assert_eq!(acknowledged.load(Ordering::Relaxed), 0);
}

#[test]
fn late_receipts_cancellation_and_lost_writer_receipts_do_not_authorize_ack() {
    for mode in 0..3 {
        let acknowledged = Arc::new(AtomicUsize::new(0));
        let start = Instant::now();
        let mut source = CheckpointSource {
            deadline: None,
            acknowledged: Arc::clone(&acknowledged),
        };
        let (send, receive) = mpsc::sync_channel(1);
        let stop = AtomicBool::new(false);
        let output = output(&send, &stop, start);
        thread::scope(|scope| {
            let worker = scope.spawn(|| {
                source.deadline = Some(
                    Instant::now()
                        + if mode == 0 {
                            Duration::from_millis(100)
                        } else {
                            Duration::from_secs(2)
                        },
                );
                output.send(&mut source, observation(1), start, None)
            });
            let batch = receive.recv_timeout(Duration::from_secs(1)).unwrap();
            if mode == 1 {
                stop.store(true, Ordering::Release);
            }
            let receipt = batch.checkpoint.unwrap();
            let held = if mode == 2 {
                drop(receipt);
                None
            } else {
                Some(receipt)
            };
            let error = worker.join().unwrap().unwrap_err();
            let expected = match mode {
                0 => ErrorKind::Timeout,
                1 => ErrorKind::Interrupted,
                _ => ErrorKind::Incomplete,
            };
            assert_eq!(classify(&error).kind, expected);
            if let Some(receipt) = held {
                receipt.try_send(()).unwrap_err();
            }
        });
        assert_eq!(acknowledged.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn legacy_delivery_remains_queue_only_even_after_stop_and_allows_idle() {
    let acknowledged = Arc::new(AtomicUsize::new(0));
    let mut source = CheckpointSource {
        deadline: None,
        acknowledged: Arc::clone(&acknowledged),
    };
    let (send, receive) = mpsc::sync_channel(1);
    let stop = AtomicBool::new(true);
    let start = Instant::now();
    let output = output(&send, &stop, start);
    output.send(&mut source, Vec::new(), start, None).unwrap();
    assert!(matches!(receive.try_recv(), Err(mpsc::TryRecvError::Empty)));
    output
        .send(&mut source, observation(7), start, None)
        .unwrap();
    let batch = receive.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(batch.checkpoint.is_none());
    assert_eq!(batch.readings[0].value, 7);
    assert_eq!(acknowledged.load(Ordering::Relaxed), 0);
}

#[test]
fn a_writer_failure_wakes_pending_source_without_ack_or_replacing_the_writer_error() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("failed.jsonl");
    let mut file = crate::storage::Recording::create(&path, Some(1), None).unwrap();
    let acknowledged = Arc::new(AtomicUsize::new(0));
    let mut source = CheckpointSource {
        deadline: Some(Instant::now() + Duration::from_secs(2)),
        acknowledged: Arc::clone(&acknowledged),
    };
    let (send, receive) = mpsc::sync_channel(1);
    let stop = AtomicBool::new(false);
    let start = Instant::now();
    let output = output(&send, &stop, start);
    let mut samples = 0;
    thread::scope(|scope| {
        let worker = scope.spawn(|| output.send(&mut source, observation(9), start, None));
        let batch = receive.recv_timeout(Duration::from_secs(1)).unwrap();
        let error = super::write_batch(&mut file, batch, &mut samples, chrono::Utc::now(), &stop)
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::FileTooLarge
        );
        assert!(stop.load(Ordering::Acquire));
        let kind = classify(&worker.join().unwrap().unwrap_err()).kind;
        assert!(matches!(
            kind,
            ErrorKind::Incomplete | ErrorKind::Interrupted
        ));
    });
    assert_eq!(samples, 0);
    assert_eq!(acknowledged.load(Ordering::Relaxed), 0);
    file.finish().unwrap();
}
