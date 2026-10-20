use super::*;
use crate::types::RGB8;
use std::{fs, sync::atomic::AtomicBool};

/// A temporary folder of media and a session file beside it.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "capturefab-folder-review-{name}-{}-{}",
            std::process::id(),
            epoch_ms()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("media")).unwrap();
        Self { root }
    }
    fn folder(&self) -> PathBuf {
        self.root.join("media")
    }
    fn session(&self) -> PathBuf {
        self.root.join("review.duckdb")
    }
    /// A PNG split into a left and right color, so images differ in layout
    /// as well as color.
    fn image(&self, name: &str, left: [u8; 3], right: [u8; 3]) -> PathBuf {
        let path = self.folder().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, png(left, right)).unwrap();
        path
    }
    fn native(&self) -> Options {
        Options {
            encoder: Some(EncoderChoice::Native),
            ..Options::new(self.folder())
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn png(left: [u8; 3], right: [u8; 3]) -> Vec<u8> {
    let (width, height) = (32u32, 24u32);
    let mut data = Vec::with_capacity((width * height * 3) as usize);
    for _ in 0..height {
        for x in 0..width {
            data.extend(if x < width / 2 { left } else { right });
        }
    }
    crate::frame::encode(
        &Frame {
            id: 0,
            width,
            height,
            pixel_format: RGB8,
            timestamp_ns: 0,
            data,
        },
        "png",
    )
    .unwrap()
}

fn scan(session: &mut ReviewSession) -> ScanSummary {
    session.scan(&AtomicBool::new(false), &mut |_| {}).unwrap()
}

/// Gray images that differ a little, and one that stands out.
fn grays_and_red(fixture: &Fixture) {
    for (i, value) in [100u8, 108, 116, 124, 132].into_iter().enumerate() {
        fixture.image(
            &format!("gray-{i}.png"),
            [value, value, value],
            [value + 40, value + 40, value + 40],
        );
    }
    fixture.image("nested/red.png", [230, 20, 20], [250, 60, 30]);
}

#[test]
fn walk_skips_hidden_entries_links_and_other_files() {
    let fixture = Fixture::new("walk");
    fixture.image("a.png", [1, 2, 3], [4, 5, 6]);
    fixture.image("deep/er/b.jpg", [1, 2, 3], [4, 5, 6]);
    fixture.image(".hidden/c.png", [1, 2, 3], [4, 5, 6]);
    fixture.image(".d.png", [1, 2, 3], [4, 5, 6]);
    fs::write(fixture.folder().join("notes.txt"), "not media").unwrap();
    fs::write(fixture.folder().join("clip.mp4"), "not decoded by a walk").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        fixture.folder().join("a.png"),
        fixture.folder().join("link.png"),
    )
    .unwrap();
    let walk = scan::walk(&fixture.folder()).unwrap();
    let paths: Vec<_> = walk.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["a.png", "clip.mp4", "deep/er/b.jpg"]);
    assert_eq!(walk.files[1].kind, Kind::Video);
    assert!(scan::source(&fixture.folder(), "../escape.png").is_err());
    assert!(scan::source(&fixture.folder(), "/etc/passwd").is_err());
}

#[test]
fn novelty_offers_the_atypical_item_first_and_learns_from_decisions() {
    let fixture = Fixture::new("novelty");
    grays_and_red(&fixture);
    let mut session = ReviewSession::create(&fixture.session(), &fixture.native()).unwrap();
    assert_eq!(session.status().unwrap().rank, Rank::Novelty);
    let summary = scan(&mut session);
    assert_eq!((summary.files, summary.added, summary.failed), (6, 6, 0));
    let first = session.next(true).unwrap().unwrap();
    assert_eq!(first.path, "nested/red.png");
    assert_eq!(first.kind, "image");
    assert_eq!((first.width, first.height), (32, 24));
    assert!(first.reason.contains("typical item"), "{}", first.reason);
    session.review(first.id, false).unwrap();
    // A repeated or stale decision changes nothing.
    assert!(session.review(first.id, true).is_err());
    let second = session.next(true).unwrap().unwrap();
    assert_ne!(second.id, first.id);
    assert!(
        second.reason.contains("everything reviewed"),
        "{}",
        second.reason
    );
    let status = session.status().unwrap();
    assert_eq!((status.pending, status.discarded), (5, 1));
    assert_eq!(session.frame(second.id).unwrap().width, 32);
}

#[test]
fn ranking_survives_reopening() {
    let fixture = Fixture::new("reopen");
    grays_and_red(&fixture);
    let mut session = ReviewSession::create(&fixture.session(), &fixture.native()).unwrap();
    scan(&mut session);
    let first = session.next(true).unwrap().unwrap();
    session.review(first.id, true).unwrap();
    let expected = session.next(true).unwrap().unwrap();
    session.close().unwrap();
    let mut session = ReviewSession::open(&fixture.session()).unwrap();
    assert_eq!(session.next(true).unwrap().unwrap(), expected);
    assert_eq!(session.status().unwrap().kept, 1);
}

#[test]
fn repeats_leave_the_queue() {
    let fixture = Fixture::new("repeats");
    fixture.image("a.png", [10, 200, 10], [10, 220, 10]);
    fixture.image("b.png", [10, 200, 10], [10, 220, 10]);
    fixture.image("c.png", [200, 10, 200], [10, 10, 220]);
    let mut session = ReviewSession::create(&fixture.session(), &fixture.native()).unwrap();
    let summary = scan(&mut session);
    assert_eq!((summary.items, summary.duplicates), (3, 1));
    assert_eq!(session.status().unwrap().pending, 2);
    // A decision also takes pending look-alikes out of the queue.
    fixture.image("d.png", [200, 10, 200], [10, 10, 221]);
    fs::remove_file(fixture.folder().join("a.png")).unwrap();
    fixture.image("e.png", [30, 30, 90], [90, 30, 30]);
    let summary = scan(&mut session);
    assert_eq!(
        (summary.added, summary.missing, summary.unchanged),
        (2, 1, 2)
    );
    let status = session.status().unwrap();
    // b.png repeated a.png, which is gone, so b.png is pending again.
    assert_eq!(
        (status.missing_files, status.duplicates, status.pending),
        (1, 0, 4)
    );
    session.review(id_of(&session, "c.png"), true).unwrap();
    let status = session.status().unwrap();
    assert_eq!((status.duplicates, status.pending), (1, 2), "{status:?}");
    let d = id_of(&session, "d.png");
    assert!(session.review(d, false).is_err());
}

fn id_of(session: &ReviewSession, path: &str) -> u64 {
    session
        .db
        .query_row(
            "SELECT i.id FROM items i JOIN files f ON f.id = i.file_id WHERE f.path = ?",
            params![path],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn changed_and_vanished_files_are_sampled_again_or_dropped() {
    let fixture = Fixture::new("changes");
    grays_and_red(&fixture);
    let mut session = ReviewSession::create(&fixture.session(), &fixture.native()).unwrap();
    scan(&mut session);
    let first = session.next(true).unwrap().unwrap();
    session.review(first.id, true).unwrap();
    // A pending item whose file changed is dropped with a note.
    let second = session.next(true).unwrap().unwrap();
    let path = fixture.folder().join(&second.path);
    fs::write(&path, png([0, 0, 255], [0, 255, 255])).unwrap();
    let file = fs::File::options().write(true).open(&path).unwrap();
    file.set_modified(SystemTime::now() + std::time::Duration::from_secs(5))
        .unwrap();
    assert!(session.frame(second.id).is_err());
    assert_ne!(session.next(true).unwrap().unwrap().id, second.id);
    assert!(session.status().unwrap().note.unwrap().contains("changed"));
    let summary = scan(&mut session);
    assert_eq!((summary.changed, summary.items), (1, 1));
    // The kept item stays kept.
    assert_eq!(session.status().unwrap().kept, 1);
}

#[test]
fn a_stopped_scan_keeps_finished_files_and_resumes() {
    let fixture = Fixture::new("stop");
    grays_and_red(&fixture);
    let mut session = ReviewSession::create(&fixture.session(), &fixture.native()).unwrap();
    let stop = AtomicBool::new(false);
    let summary = session
        .scan(&stop, &mut |progress| {
            if progress.done == 2 {
                stop.store(true, Ordering::Relaxed);
            }
        })
        .unwrap();
    assert!(summary.stopped);
    assert_eq!(session.status().unwrap().items, 2);
    let summary = scan(&mut session);
    assert_eq!((summary.unchanged, summary.added), (2, 4));
}

#[test]
fn unreadable_files_are_recorded_and_retried() {
    let fixture = Fixture::new("broken");
    fixture.image("good.png", [1, 2, 3], [200, 100, 50]);
    fs::write(
        fixture.folder().join("broken.png"),
        b"\x89PNG\r\n\x1a\nbroken",
    )
    .unwrap();
    let mut session = ReviewSession::create(&fixture.session(), &fixture.native()).unwrap();
    let summary = scan(&mut session);
    assert_eq!((summary.failed, summary.items), (1, 1));
    assert_eq!(summary.errors[0].path, "broken.png");
    assert_eq!(session.status().unwrap().failed_files, 1);
    fixture.image("broken.png", [9, 9, 9], [90, 90, 90]);
    let summary = scan(&mut session);
    assert_eq!((summary.changed, summary.failed), (1, 0));
}

#[test]
fn similar_ranks_by_taught_examples() {
    let fixture = Fixture::new("similar");
    grays_and_red(&fixture);
    fixture.image("blue.png", [20, 20, 230], [30, 60, 250]);
    let examples = fixture.root.join("examples.duckdb");
    {
        let mut capture = crate::autocapture::CaptureSession::open(
            &examples,
            "sim:0",
            true,
            Some(EncoderChoice::Native),
        )
        .unwrap();
        let blue = crate::hubs::image::decode(&png([20, 20, 228], [30, 62, 250])).unwrap();
        capture.manual(&blue).unwrap();
        capture.close().unwrap();
    }
    let options = Options {
        encoder: None,
        examples: Some(examples.clone()),
        ..Options::new(fixture.folder())
    };
    let mut session = ReviewSession::create(&fixture.session(), &options).unwrap();
    let status = session.status().unwrap();
    assert_eq!(
        (status.rank, status.examples, status.encoder.as_str()),
        (Rank::Similar, 1, "native")
    );
    scan(&mut session);
    let first = session.next(false).unwrap().unwrap();
    assert_eq!(first.path, "blue.png");
    assert!(
        first.reason.contains("match to kept items"),
        "{}",
        first.reason
    );
    session.review(first.id, true).unwrap();
    // Nothing else looks like the example.
    assert_eq!(session.next(false).unwrap(), None);
    assert!(session.next(true).unwrap().is_some());
    // Novelty now ranks what is left by its distance from the kept item.
    session.configure(Some(Rank::Novelty), Some(1.0)).unwrap();
    let next = session.next(false).unwrap().unwrap();
    assert!(
        next.reason.contains("against everything reviewed"),
        "{}",
        next.reason
    );
    #[cfg(feature = "dinov2")]
    {
        let options = Options {
            encoder: Some(EncoderChoice::Dinov2),
            examples: Some(examples),
            ..Options::new(fixture.folder())
        };
        let other = fixture.root.join("other.duckdb");
        let error = ReviewSession::create(&other, &options).err().unwrap();
        assert!(
            format!("{error:#}").contains("--encoder native"),
            "{error:#}"
        );
        assert!(!other.exists());
    }
}

#[test]
fn export_copies_kept_items_without_replacing_anything() {
    let fixture = Fixture::new("export");
    grays_and_red(&fixture);
    let mut session = ReviewSession::create(&fixture.session(), &fixture.native()).unwrap();
    scan(&mut session);
    let red = session.next(true).unwrap().unwrap();
    session.review(red.id, true).unwrap();
    let out = fixture.root.join("out");
    fs::create_dir_all(out.join("nested")).unwrap();
    fs::write(out.join("nested/red.png"), b"someone else's").unwrap();
    let target = crate::destination::resolve(None, out.to_str().unwrap()).unwrap();
    let summary = session.export(&target).unwrap();
    assert_eq!(summary.exported.len(), 1);
    let exported = PathBuf::from(&summary.exported[0].path);
    assert_eq!(exported, out.join("nested/red-1.png"));
    assert_eq!(
        fs::read(&exported).unwrap(),
        fs::read(fixture.folder().join("nested/red.png")).unwrap()
    );
    assert_eq!(
        fs::read(out.join("nested/red.png")).unwrap(),
        b"someone else's"
    );
    // Exported items are not exported twice.
    assert!(session.export(&target).unwrap().exported.is_empty());
    assert_eq!(session.status().unwrap().exported, 1);
}

#[test]
fn sessions_refuse_bad_paths_settings_and_reuse() {
    let fixture = Fixture::new("refusals");
    assert!(ReviewSession::create(&fixture.root.join("review.db"), &fixture.native()).is_err());
    let missing = Options::new(fixture.root.join("missing"));
    let error = ReviewSession::create(&fixture.session(), &missing)
        .err()
        .unwrap();
    assert_eq!(crate::cli::error_code(&error).0, "unavailable");
    assert!(!fixture.session().exists());
    let bad = Options {
        sensitivity: 1.5,
        ..fixture.native()
    };
    assert!(ReviewSession::create(&fixture.session(), &bad).is_err());
    let bad = Options {
        video_interval_ms: 10,
        ..fixture.native()
    };
    assert!(ReviewSession::create(&fixture.session(), &bad).is_err());
    ReviewSession::create(&fixture.session(), &fixture.native())
        .unwrap()
        .close()
        .unwrap();
    assert!(ReviewSession::create(&fixture.session(), &fixture.native()).is_err());
    assert!(ReviewSession::open(&fixture.root.join("none.duckdb")).is_err());
}

/// Six one-second frames: red three times, blue twice, then green.
fn clip(path: &Path) {
    use std::io::Write;
    use std::process::Stdio;
    let executable = crate::media::executable().unwrap();
    let mut child = crate::media::command(&executable)
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "rawvideo",
        ])
        .args([
            "-pixel_format",
            "rgb24",
            "-video_size",
            "32x18",
            "-framerate",
            "1",
        ])
        .args(["-i", "pipe:0", "-c:v", "ffv1"])
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for color in [[220, 20, 20]; 3]
        .into_iter()
        .chain([[20, 20, 220]; 2])
        .chain([[20, 220, 20]])
    {
        for _ in 0..32 * 18 {
            input.write_all(&color).unwrap();
        }
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "needs FFmpeg"]
fn videos_are_sampled_shown_and_exported_by_offset() {
    let fixture = Fixture::new("video");
    clip(&fixture.folder().join("clip.mkv"));
    let options = Options {
        video_interval_ms: 1_000,
        ..fixture.native()
    };
    let mut session = ReviewSession::create(&fixture.session(), &options).unwrap();
    let summary = scan(&mut session);
    assert_eq!(
        (summary.failed, summary.items, summary.duplicates),
        (0, 6, 3),
        "{summary:?}"
    );
    let mut seen = Vec::new();
    while let Some(item) = session.next(true).unwrap() {
        assert_eq!(item.kind, "video");
        let frame = session.frame(item.id).unwrap();
        let center = &frame.data[(frame.data.len() / 2 / 3) * 3..][..3];
        seen.push((
            item.offset_ms.unwrap(),
            center.iter().position(|v| *v > 150),
        ));
        session.review(item.id, true).unwrap();
    }
    seen.sort();
    assert_eq!(seen, [(0, Some(0)), (3_000, Some(2)), (5_000, Some(1))]);
    let out = fixture.root.join("out");
    let target = crate::destination::resolve(None, out.to_str().unwrap()).unwrap();
    let summary = session.export(&target).unwrap();
    let mut names: Vec<_> = summary
        .exported
        .iter()
        .map(|e| {
            PathBuf::from(&e.path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["clip-0ms.png", "clip-3000ms.png", "clip-5000ms.png"]
    );
}

/// Make `path` look modified: new content and a later modification time.
fn touch(path: &Path, left: [u8; 3], right: [u8; 3]) {
    fs::write(path, png(left, right)).unwrap();
    let file = fs::File::options().write(true).open(path).unwrap();
    file.set_modified(SystemTime::now() + std::time::Duration::from_secs(5))
        .unwrap();
}

#[test]
fn a_kept_item_whose_file_changed_is_never_exported() {
    let fixture = Fixture::new("kept-changed");
    grays_and_red(&fixture);
    let mut session = ReviewSession::create(&fixture.session(), &fixture.native()).unwrap();
    scan(&mut session);
    let red = session.next(true).unwrap().unwrap();
    session.review(red.id, true).unwrap();
    touch(
        &fixture.folder().join(&red.path),
        [0, 0, 0],
        [255, 255, 255],
    );
    // Even after a rescan records the new content, the decision was about
    // the old one.
    scan(&mut session);
    let out = fixture.root.join("out");
    let target = crate::destination::resolve(None, out.to_str().unwrap()).unwrap();
    let summary = session.export(&target).unwrap();
    assert!(summary.exported.is_empty(), "{summary:?}");
    assert_eq!(summary.skipped[0].path, red.path);
    assert!(!out.join(&red.path).exists());
}

#[test]
fn repeats_of_a_dropped_item_are_pending_again() {
    let fixture = Fixture::new("dropped-original");
    fixture.image("a.png", [10, 200, 10], [10, 220, 10]);
    fixture.image("b.png", [10, 200, 10], [10, 220, 10]);
    let mut session = ReviewSession::create(&fixture.session(), &fixture.native()).unwrap();
    assert_eq!(scan(&mut session).duplicates, 1);
    touch(
        &fixture.folder().join("a.png"),
        [200, 10, 10],
        [220, 10, 10],
    );
    // a.png changed before it was reviewed, so b.png takes its place.
    let item = session.next(true).unwrap().unwrap();
    assert_eq!(item.path, "b.png");
    let status = session.status().unwrap();
    assert_eq!((status.pending, status.duplicates), (1, 0));
}
