//! Three-slot, single-producer/single-consumer frame ring shared across processes.
//! Slot ownership uses acquire/release AtomicU32 transitions; pixels never travel in JSON.
use crate::types::Frame;
use anyhow::{Result, ensure};
use memmap2::MmapMut;
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
};
const HEADER: usize = 64;
const META: usize = 64;
const SLOTS: usize = 3;
const MAGIC: &[u8; 8] = b"CFABRNG1";
const FREE: u32 = 0;
const WRITING: u32 = 1;
const READY: u32 = 2;
const READING: u32 = 3;
pub const DEFAULT_CAPACITY: usize = 16 * 1024 * 1024;

pub struct SharedRing {
    map: MmapMut,
    capacity: usize,
    path: PathBuf,
    _file: File,
    owner: bool,
}
impl SharedRing {
    pub fn create(path: &Path, capacity: usize) -> Result<Self> {
        ensure!(
            (4096..=256 * 1024 * 1024).contains(&capacity) && capacity.is_multiple_of(64),
            "ring capacity must be aligned to 64 bytes, 4096..268435456"
        );
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        file.set_len((HEADER + SLOTS * (META + capacity)) as u64)?;
        // SAFETY: file length remains fixed for the ring lifetime. Private file, fixed layout.
        let mut map = unsafe { MmapMut::map_mut(&file) }?;
        map[..HEADER].fill(0);
        map[..8].copy_from_slice(MAGIC);
        map[8..16].copy_from_slice(&(capacity as u64).to_le_bytes());
        for i in 0..SLOTS {
            map[HEADER + i * (META + capacity)..HEADER + i * (META + capacity) + META].fill(0);
        }
        Ok(Self {
            map,
            capacity,
            path: path.into(),
            _file: file,
            owner: true,
        })
    }
    pub fn open(path: &Path) -> Result<Self> {
        let meta = std::fs::symlink_metadata(path)?;
        ensure!(
            meta.is_file() && !meta.file_type().is_symlink(),
            "ring must be a regular file"
        );
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        ensure!(
            file.metadata()?.len() >= HEADER as u64,
            "truncated shared ring"
        );
        // SAFETY: creator owns this private file and never resizes it while workers run.
        let map = unsafe { MmapMut::map_mut(&file) }?;
        ensure!(&map[..8] == MAGIC, "shared ring version mismatch");
        let capacity = usize::try_from(u64::from_le_bytes(map[8..16].try_into()?))?;
        ensure!(
            (4096..=256 * 1024 * 1024).contains(&capacity)
                && capacity.is_multiple_of(64)
                && map.len() == HEADER + SLOTS * (META + capacity),
            "invalid shared ring layout"
        );
        Ok(Self {
            map,
            capacity,
            path: path.into(),
            _file: file,
            owner: false,
        })
    }
    fn base(&self, slot: usize) -> usize {
        HEADER + slot * (META + self.capacity)
    }
    fn atomic(&self, offset: usize) -> &AtomicU32 {
        assert!(offset.is_multiple_of(4) && offset + 4 <= self.map.len());
        // SAFETY: mmap is page aligned, all atomic offsets are 4-byte aligned and
        // exclusively accessed atomically after initial construction. Both processes
        // use the same native-endian AtomicU32 ABI; state guards all non-atomic bytes.
        unsafe { &*self.map.as_ptr().add(offset).cast::<AtomicU32>() }
    }
    pub fn sequence(&self) -> u32 {
        self.atomic(16).load(Ordering::Acquire)
    }
    pub fn dropped(&self) -> u32 {
        self.atomic(20).load(Ordering::Acquire)
    }
    pub fn cancel(&self) {
        self.atomic(24).store(1, Ordering::Release);
    }
    pub fn cancelled(&self) -> bool {
        self.atomic(24).load(Ordering::Acquire) != 0
    }
    pub fn write(&mut self, frame: &Frame) -> Result<bool> {
        ensure!(
            frame.data.len() <= self.capacity,
            "frame exceeds shared-memory capacity ({} bytes); increase CAPTUREFAB_FRAME_BYTES",
            self.capacity
        );
        let mut selected = None;
        for i in 0..SLOTS {
            if self
                .atomic(self.base(i))
                .compare_exchange(FREE, WRITING, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                selected = Some(i);
                break;
            }
        }
        if selected.is_none() {
            for i in 0..SLOTS {
                if self
                    .atomic(self.base(i))
                    .compare_exchange(READY, WRITING, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    selected = Some(i);
                    self.atomic(20).fetch_add(1, Ordering::Relaxed);
                    break;
                }
            }
        }
        let Some(slot) = selected else {
            self.atomic(20).fetch_add(1, Ordering::Relaxed);
            return Ok(false);
        };
        let seq = self.sequence().wrapping_add(1);
        let b = self.base(slot);
        self.map[b + 4..b + 8].copy_from_slice(&seq.to_le_bytes());
        self.map[b + 8..b + 16].copy_from_slice(&frame.id.to_le_bytes());
        self.map[b + 16..b + 20].copy_from_slice(&frame.width.to_le_bytes());
        self.map[b + 20..b + 24].copy_from_slice(&frame.height.to_le_bytes());
        self.map[b + 24..b + 28].copy_from_slice(&frame.pixel_format.to_le_bytes());
        self.map[b + 28..b + 32].copy_from_slice(&(frame.data.len() as u32).to_le_bytes());
        self.map[b + 32..b + 40].copy_from_slice(&frame.timestamp_ns.to_le_bytes());
        self.map[b + META..b + META + frame.data.len()].copy_from_slice(&frame.data);
        self.atomic(b).store(READY, Ordering::Release);
        self.atomic(16).store(seq, Ordering::Release);
        Ok(true)
    }
    pub fn take_latest(&mut self) -> Result<Option<Frame>> {
        let mut newest: Option<(usize, u32)> = None;
        // Claim before inspecting non-atomic metadata. A writer may reclaim READY slots.
        for i in 0..SLOTS {
            let b = self.base(i);
            if self
                .atomic(b)
                .compare_exchange(READY, READING, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                let seq = u32::from_le_bytes(self.map[b + 4..b + 8].try_into()?);
                if newest.is_none_or(|(_, old)| seq.wrapping_sub(old) < u32::MAX / 2) {
                    if let Some((old, _)) = newest {
                        self.atomic(self.base(old)).store(FREE, Ordering::Release);
                    }
                    newest = Some((i, seq));
                } else {
                    self.atomic(b).store(FREE, Ordering::Release);
                }
            }
        }
        let Some((slot, _)) = newest else {
            return Ok(None);
        };
        let b = self.base(slot);
        let result = (|| -> Result<Frame> {
            let length = u32::from_le_bytes(self.map[b + 28..b + 32].try_into()?) as usize;
            ensure!(length <= self.capacity, "corrupt ring frame length");
            Ok(Frame {
                id: u64::from_le_bytes(self.map[b + 8..b + 16].try_into()?),
                width: u32::from_le_bytes(self.map[b + 16..b + 20].try_into()?),
                height: u32::from_le_bytes(self.map[b + 20..b + 24].try_into()?),
                pixel_format: u32::from_le_bytes(self.map[b + 24..b + 28].try_into()?),
                timestamp_ns: u64::from_le_bytes(self.map[b + 32..b + 40].try_into()?),
                data: self.map[b + META..b + META + length].to_vec(),
            })
        })();
        self.atomic(b).store(FREE, Ordering::Release);
        Ok(Some(result?))
    }
}
impl Drop for SharedRing {
    fn drop(&mut self) {
        if self.owner {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mapped_ring_roundtrip_and_bounds() {
        let path = std::env::temp_dir().join(format!(
            "capturefab-ring-test-{}-{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut writer = SharedRing::create(&path, 4096).unwrap();
        let mut reader = SharedRing::open(&path).unwrap();
        let mut f = Frame {
            id: 1,
            width: 8,
            height: 8,
            pixel_format: crate::types::MONO8,
            timestamp_ns: 123,
            data: vec![7; 64],
        };
        for id in 1..=5 {
            f.id = id;
            writer.write(&f).unwrap();
        }
        assert_eq!(reader.take_latest().unwrap().unwrap().id, 5);
        assert!(reader.take_latest().unwrap().is_none());
        assert!(writer.dropped() > 0);
        f.data = vec![0; 4097];
        assert!(writer.write(&f).is_err());
        drop(reader);
        drop(writer);
        assert!(!path.exists());
    }
}
