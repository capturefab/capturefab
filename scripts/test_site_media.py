#!/usr/bin/env python3
"""Unit tests for the website clip recorder and the site's clip checks."""
import importlib.util
import json
import struct
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

SCRIPTS = Path(__file__).resolve().parent


def load(name):
    spec = importlib.util.spec_from_file_location(name.replace('-', '_'), SCRIPTS / f'{name}.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


media = load('site-media')
site = load('test-site')

try:
    import PIL  # noqa: F401
    HAVE_PIL = True
except ImportError:
    HAVE_PIL = False


def box(kind, body):
    return struct.pack('>I4s', 8 + len(body), kind) + body


def full(kind, version, body):
    return box(kind, bytes([version, 0, 0, 0]) + body)


def mp4(samples=240, delta=512, timescale=15360, sizes=None, width=1100, height=700,
        mdhd_version=0, stts=None, codec=b'avc1'):
    """A minimal MP4: ftyp, then moov with one video trak, then mdat."""
    duration = samples * delta
    tkhd = full(b'tkhd', 0, bytes(72) + struct.pack('>II', width << 16, height << 16))
    if mdhd_version == 1:
        mdhd = full(b'mdhd', 1, struct.pack('>QQIQ', 0, 0, timescale, duration) + bytes(4))
    else:
        mdhd = full(b'mdhd', 0, struct.pack('>IIII', 0, 0, timescale, duration) + bytes(4))
    hdlr = full(b'hdlr', 0, bytes(4) + b'vide' + bytes(12) + b'Video\0')
    stsd = full(b'stsd', 0, struct.pack('>I', 1) + box(codec, bytes(78)))
    entries = stts or [(samples, delta)]
    stts_box = full(b'stts', 0, struct.pack('>I', len(entries)) + b''.join(struct.pack('>II', *e) for e in entries))
    sizes = [2000] * samples if sizes is None else sizes
    stsz = full(b'stsz', 0, struct.pack('>II', 0, len(sizes)) + b''.join(struct.pack('>I', s) for s in sizes))
    stbl = box(b'stbl', stsd + stts_box + stsz)
    mdia = box(b'mdia', mdhd + hdlr + box(b'minf', stbl))
    moov = box(b'moov', box(b'trak', tkhd + mdia))
    return box(b'ftyp', b'isom' + bytes(4)) + moov + box(b'mdat', bytes(16))


def clip_entry(path, **overrides):
    clip = {'fps': 30, 'duration_seconds': 8.0, 'frame_count': 240, 'width': 1100, 'height': 700,
            'size_bytes': path.stat().st_size, 'preview_motion_fps': 29.0,
            'source': {'observed_fps': 28.0}}
    clip.update(overrides)
    return clip


class Mp4TimingTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)

    def tearDown(self):
        self.directory.cleanup()

    def write(self, data, name='clip.mp4'):
        path = self.root / name
        path.write_bytes(data)
        return path

    def test_reads_video_timing_with_both_mdhd_versions(self):
        for version in (0, 1):
            timing = site.mp4_video_timing(self.write(mp4(mdhd_version=version)))
            self.assertEqual((timing['width'], timing['height'], timing['codec']), (1100, 700, 'avc1'))
            self.assertEqual((timing['timescale'], timing['duration']), (15360, 240 * 512))
            self.assertEqual(timing['stts'], [(240, 512)])
            self.assertEqual(timing['samples'], 240)
            self.assertEqual(len(timing['sizes']), 240)

    def errors(self, data, **overrides):
        path = self.write(data)
        errors = []
        site.check_clip_timing('media/clip.mp4', path, clip_entry(path, **overrides), errors)
        return errors

    def test_passing_clip_has_no_errors(self):
        self.assertEqual(self.errors(mp4()), [])

    def test_frame_rate_mismatch(self):
        errors = self.errors(mp4(samples=192, delta=640), frame_count=192)
        self.assertIn('media/clip.mp4: video plays at 24.0 fps; manifest says 30', errors)

    def test_low_manifest_rate(self):
        errors = self.errors(mp4(samples=192, delta=640), frame_count=192, fps=24.0 - 1)
        self.assertIn('media/clip.mp4: demonstration must play at 24 fps or more', errors)

    def test_variable_frame_rate(self):
        errors = self.errors(mp4(stts=[(120, 512), (120, 512)]))
        self.assertIn('media/clip.mp4: expected constant-frame-rate video', errors)

    def test_mostly_repeated_frames(self):
        sizes = [2000 if index % 8 == 0 else 60 for index in range(240)]
        errors = self.errors(mp4(sizes=sizes))
        self.assertIn('media/clip.mp4: only 3.8 fps of the 30 fps clip show new images (repeated frames)', errors)

    def test_size_mismatch_and_clip_budget(self):
        path = self.write(mp4() + bytes(1_500_001))
        errors = []
        site.check_clip_timing('media/clip.mp4', path, clip_entry(path, size_bytes=5), errors)
        self.assertTrue(any('manifest says 5' in error for error in errors), errors)
        self.assertTrue(any('1,500,000-byte clip budget' in error for error in errors), errors)

    def test_low_capture_and_preview_rates(self):
        errors = self.errors(mp4(), preview_motion_fps=3.0, source={'observed_fps': 2.8})
        self.assertIn('media/clip.mp4: renderer capture rate must be 24 fps or more', errors)
        self.assertIn('media/clip.mp4: camera preview change rate must be 24 fps or more', errors)

    def test_total_budget(self):
        media_dir = self.root / 'media'
        media_dir.mkdir()
        for name in ('a.mp4', 'b.mp4', 'c.mp4'):
            (media_dir / name).write_bytes(bytes(1_200_000))
        (media_dir / 'manifest.json').write_text(json.dumps({'clips': []}))
        errors = []
        site.check_media(self.root, {}, errors)
        self.assertTrue(any('3,500,000-byte budget' in error for error in errors), errors)


def entry(index, staged_ms, preview_frame=None, held=0):
    return {'index': index, 'file': f'frame-{index:04d}.png', 'staged_ms': staged_ms,
            'read_ms': staged_ms + 30, 'preview_frame': index if preview_frame is None else preview_frame,
            'camera_frames': {'sim:0': index}, 'held_by_pending': held}


class TimingReportTests(unittest.TestCase):
    def steady(self, count=301, interval=1000 / 30):
        return [entry(index, index * interval) for index in range(count)]

    def test_passes_at_thirty(self):
        report = media.timing_report('live-preview', self.steady(), 8)
        self.assertAlmostEqual(report['observed_fps'], 30, places=1)
        self.assertLessEqual(report['max_frame_gap_ms'], 34)

    def test_low_rate_fails(self):
        with self.assertRaisesRegex(media.MediaError, r'live-preview: renderer captured 20\.0 fps over the 8\.0 s excerpt'):
            media.timing_report('live-preview', self.steady(interval=50), 8)

    def test_long_gap_fails(self):
        entries = self.steady()
        for item in entries[100:]:
            item['staged_ms'] += 87
        with self.assertRaisesRegex(media.MediaError, r'120 ms gap.*longest gap 120 ms at 3\.30 s'):
            media.timing_report('live-preview', entries, 8)

    def test_stale_preview_fails(self):
        entries = [dict(item, preview_frame=item['index'] // 2) for item in self.steady()]
        with self.assertRaisesRegex(media.MediaError, r'distinct camera frames'):
            media.timing_report('live-preview', entries, 8)

    def test_frames_must_cover_the_excerpt(self):
        with self.assertRaisesRegex(media.MediaError, r'increase --frames'):
            media.timing_report('live-preview', self.steady(count=200), 8)

    def test_first_frame_interval_is_excluded(self):
        # Frame 0 waits for one-time scene setup; it is dropped before timing.
        raw = [entry(0, 0)] + [entry(index, 900 + index * 1000 / 30) for index in range(1, 302)]
        rebased = media.rebase(raw)
        self.assertEqual(rebased[0]['index'], 1)
        self.assertEqual(rebased[0]['staged_ms'], 0)
        media.timing_report('live-preview', rebased, 8)

    def test_concat_listing_uses_staging_times(self):
        entries = [entry(1, 0), entry(2, 40), entry(3, 70)]
        listing = media.concat_listing(entries, Path('/frames'))
        self.assertEqual(listing, "file '/frames/frame-0001.png'\noption framerate 90000\nduration 0.040000\n"
                                  "file '/frames/frame-0002.png'\noption framerate 90000\nduration 0.030000\n"
                                  "file '/frames/frame-0003.png'\noption framerate 90000\nduration 0.040000\n")
        with self.assertRaises(media.MediaError):
            media.concat_listing([entry(1, 0), entry(2, 0)], Path('/frames'))


class SidecarTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)

    def tearDown(self):
        self.directory.cleanup()

    def write(self, entries):
        for item in entries:
            (self.root / Path(item['file']).name).write_bytes(b'png')
        path = self.root / 'frame-timing.jsonl'
        path.write_text(''.join(json.dumps(item) + '\n' for item in entries))
        return path

    def test_accepts_every_index_once(self):
        path = self.write([entry(1, 33), entry(0, 0)])
        self.assertEqual([item['index'] for item in media.parse_sidecar(path, 2, self.root)], [0, 1])

    def test_rejects_missing_and_duplicate_indexes(self):
        with self.assertRaisesRegex(media.MediaError, r'missing \[1\]'):
            media.parse_sidecar(self.write([entry(0, 0)]), 2, self.root)
        with self.assertRaisesRegex(media.MediaError, r'duplicated \[0\]'):
            media.parse_sidecar(self.write([entry(0, 0), entry(0, 5)]), 2, self.root)

    def test_rejects_paths(self):
        path = self.write([entry(0, 0), dict(entry(1, 33), file='../frame-0001.png')])
        with self.assertRaisesRegex(media.MediaError, 'non-local'):
            media.parse_sidecar(path, 2, self.root)

    def test_ignores_an_incomplete_last_line(self):
        path = self.root / 'timing.jsonl'
        path.write_text(json.dumps(entry(0, 0)) + '\n{"index": 1')
        self.assertEqual(len(media.read_sidecar_lines(path)), 1)


class FfmpegTests(unittest.TestCase):
    def test_resolution_order(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            pinned = root / 'target/ffmpeg/aarch64-apple-darwin/bin/ffmpeg'
            self.assertEqual(media.resolve_ffmpeg('/x/ffmpeg', {'CAPTUREFAB_FFMPEG': '/y'}, root, 'aarch64-apple-darwin'),
                             Path('/x/ffmpeg'))
            self.assertEqual(media.resolve_ffmpeg(None, {'CAPTUREFAB_FFMPEG': '/y'}, root, 'aarch64-apple-darwin'),
                             Path('/y'))
            with self.assertRaisesRegex(media.MediaError, 'build-ffmpeg.sh'):
                media.resolve_ffmpeg(None, {}, root, 'aarch64-apple-darwin')
            pinned.parent.mkdir(parents=True)
            pinned.write_bytes(b'')
            self.assertEqual(media.resolve_ffmpeg(None, {}, root, 'aarch64-apple-darwin'), pinned)

    def runner(self, buildconf, encoders=' V....D libx264   libx264 H.264\n', version=None):
        version = version or f'ffmpeg version {media.pinned_ffmpeg_version()} Copyright\n'

        def run(command, **_):
            output = {'-version': version, '-buildconf': buildconf, '-encoders': encoders}[command[-1]]
            return subprocess.CompletedProcess(command, 0, output, '')
        return run

    def test_refuses_nonfree_and_requires_x264(self):
        with self.assertRaisesRegex(media.MediaError, 'nonfree'):
            media.check_ffmpeg('ffmpeg', self.runner('--enable-gpl --enable-nonfree'))
        with self.assertRaisesRegex(media.MediaError, 'libx264'):
            media.check_ffmpeg('ffmpeg', self.runner('--enable-gpl', encoders=' V....D ppm  PPM\n'))

    def test_reports_whether_pinned(self):
        pinned = "--disable-ffprobe --enable-encoder='rawvideo,ppm,wrapped_avframe,ffv1,libx264'"
        self.assertTrue(media.check_ffmpeg('ffmpeg', self.runner(pinned))['ffmpeg_pinned'])
        other = media.check_ffmpeg('ffmpeg', self.runner('--enable-gpl', version='ffmpeg version 7.1 x\n'))
        self.assertFalse(other['ffmpeg_pinned'])
        self.assertEqual(other['ffmpeg_version'], 'ffmpeg version 7.1 x')

    def test_crf_ladder_stops_at_the_first_fit(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / 'clip.mp4'
            sizes = {21: 2_000_000, 23: 1_600_000, 25: 1_400_000, 27: 1_000_000}
            used = []

            def run(command, **_):
                crf = int(command[command.index('-crf') + 1])
                used.append(crf)
                output.write_bytes(bytes(sizes[crf]))
            self.assertEqual(media.encode('clip', 'ffmpeg', Path('list'), output, 8, run), 25)
            self.assertEqual(used, [21, 23, 25])
            sizes[27] = 1_800_000
            sizes[25] = 1_700_000
            with self.assertRaisesRegex(media.MediaError, r'1,800,000 bytes at crf 27 exceeds the 1,500,000-byte clip budget'):
                media.encode('capture-workflow', 'ffmpeg', Path('list'), output, 8, run)

    def test_encode_command_is_constant_rate_and_bitexact(self):
        command = media.encode_command('ffmpeg', Path('list'), 8, 21, Path('out.mp4'))
        joined = ' '.join(command)
        for part in ('fps=30', '-bf 0', '-g 60', '-keyint_min 60', '-sc_threshold 0', '-threads 4',
                     '-pix_fmt yuv420p', '-fflags +bitexact', '-flags:v +bitexact', '-an', '+faststart'):
            self.assertIn(part, joined)


class DeadlineTests(unittest.TestCase):
    def test_mirrors_the_renderer_timeout(self):
        self.assertAlmostEqual(media.sequence_timeout(271, 30), 30 + 0.25 * 271)
        self.assertAlmostEqual(media.deadline_seconds(271, 30), 117.75)
        self.assertAlmostEqual(media.sequence_timeout(10, 1), 40)
        self.assertEqual(media.default_frames(30, 8), 271)


@unittest.skipUnless(HAVE_PIL, 'Pillow is not installed')
class FrameCheckTests(unittest.TestCase):
    size = (media.CHECK_WIDTH, media.CHECK_HEIGHT)

    def frame(self, text=True):
        width, height = self.size
        pixels = bytearray(width * height)
        if text:
            for y in range(0, height, 6):
                for x in range(0, width, 3):
                    pixels[y * width + x] = 220
        return bytes(pixels)

    def test_blank_text_regions_are_found(self):
        frames = [self.frame()] * 5 + [self.frame(text=False)] + [self.frame()] * 4
        self.assertEqual(media.blank_text_frames(frames, self.size), [5])
        self.assertEqual(media.blank_text_frames([self.frame()] * 4, self.size), [])

    def test_preview_differences(self):
        width, height = self.size
        moved = bytearray(self.frame())
        for y in range(60, 100):
            for x in range(200, 300):
                moved[y * width + x] = 255
        differences = media.preview_differences([self.frame(), bytes(moved), bytes(moved)], self.size)
        self.assertGreater(differences[0], 0.5)
        self.assertEqual(differences[1], 0)


if __name__ == '__main__':
    unittest.main()
