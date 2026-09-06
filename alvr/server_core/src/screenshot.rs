//! Receives screenshots taken with the headset own capture button and stores them on this PC.
//!
//! The client sends a [`ScreenshotStart`] followed by [`ScreenshotChunk`]s over the control
//! socket. Chunking is required rather than cosmetic: the control receive loop only refreshes the
//! keepalive deadline once a whole packet has been decoded, so a multi-megabyte single packet
//! would be read as a disconnection.

use alvr_common::{anyhow::Result, info, warn};
use alvr_packets::{SCREENSHOT_MAX_SIZE, ScreenshotChunk, ScreenshotStart};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

/// Extensions we are willing to store. The headset decides the encoding; we never transcode.
const ALLOWED_EXTENSIONS: [&str; 3] = ["jpeg", "jpg", "png"];

struct PartialScreenshot {
    extension: String,
    chunk_count: u32,
    total_size: u64,
    /// Kept sparse so out-of-order or duplicate chunks are handled without extra bookkeeping.
    chunks: HashMap<u32, Vec<u8>>,
}

/// A screenshot reassembled from control socket chunks, ready to be written to disk.
pub struct CompletedScreenshot {
    pub extension: String,
    pub data: Vec<u8>,
}

/// Reassembles in-flight screenshot transfers. One instance per client connection: dropping it
/// discards partial transfers, so an interrupted upload never reaches the filesystem.
#[derive(Default)]
pub struct ScreenshotAssembler {
    transfers: HashMap<u32, PartialScreenshot>,
}

impl ScreenshotAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn handle_start(&mut self, start: ScreenshotStart) {
        let extension = start.extension.to_lowercase();

        if !ALLOWED_EXTENSIONS.contains(&extension.as_str()) {
            warn!("Ignoring headset screenshot with unsupported extension: {extension}");

            return;
        }

        if start.total_size == 0 || start.total_size > SCREENSHOT_MAX_SIZE {
            warn!(
                "Ignoring headset screenshot with out of range size: {} bytes",
                start.total_size
            );

            return;
        }

        if start.chunk_count == 0 {
            warn!("Ignoring headset screenshot announcing zero chunks");

            return;
        }

        // A restarted transfer with a reused id replaces the stale one instead of merging into it.
        self.transfers.insert(
            start.id,
            PartialScreenshot {
                extension,
                chunk_count: start.chunk_count,
                total_size: start.total_size,
                chunks: HashMap::new(),
            },
        );
    }

    /// Returns the screenshot once its last missing chunk arrives.
    pub fn handle_chunk(&mut self, chunk: ScreenshotChunk) -> Option<CompletedScreenshot> {
        let transfer = self.transfers.get_mut(&chunk.id)?;

        if chunk.chunk_index >= transfer.chunk_count {
            warn!("Dropping headset screenshot: chunk index out of range");
            self.transfers.remove(&chunk.id);

            return None;
        }

        transfer.chunks.insert(chunk.chunk_index, chunk.data);

        let received_size: u64 = transfer.chunks.values().map(|c| c.len() as u64).sum();
        if received_size > transfer.total_size {
            warn!("Dropping headset screenshot: received more data than announced");
            self.transfers.remove(&chunk.id);

            return None;
        }

        if transfer.chunks.len() as u32 != transfer.chunk_count {
            return None;
        }

        let transfer = self.transfers.remove(&chunk.id)?;

        if received_size != transfer.total_size {
            warn!("Dropping headset screenshot: size mismatch after last chunk");

            return None;
        }

        let mut data = Vec::with_capacity(transfer.total_size as usize);
        for index in 0..transfer.chunk_count {
            data.extend_from_slice(&transfer.chunks[&index]);
        }

        Some(CompletedScreenshot {
            extension: transfer.extension,
            data,
        })
    }

    #[cfg(test)]
    fn pending_count(&self) -> usize {
        self.transfers.len()
    }
}

/// Root directory for received screenshots. `setting` wins when non-empty, otherwise the
/// platform Pictures folder (`FOLDERID_Pictures` on Windows, `XDG_PICTURES_DIR` on Linux).
pub fn screenshots_root(setting: &str) -> Option<PathBuf> {
    if !setting.trim().is_empty() {
        return Some(PathBuf::from(setting.trim()));
    }

    dirs::picture_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join("Pictures")))
        .map(|pictures| pictures.join("ALVR"))
}

/// Builds `<root>/YYYY-MM/YYYY-MM-DD_HH-MM-SS.<ext>`, appending `_1`, `_2`... when a file with
/// that name already exists. Timestamps use this PC local time: the file lives here, and headset
/// clocks are routinely set to another timezone.
///
/// `exists` is injected so the naming logic can be tested without touching the filesystem.
fn build_screenshot_path(
    root: &Path,
    timestamp: &str,
    extension: &str,
    exists: &mut dyn FnMut(&Path) -> bool,
) -> PathBuf {
    let month_dir = root.join(&timestamp[..7]);

    let candidate = month_dir.join(format!("{timestamp}.{extension}"));
    if !exists(&candidate) {
        return candidate;
    }

    // Bursts land in the same second: the headset can write two captures one second apart.
    for suffix in 1..u32::MAX {
        let candidate = month_dir.join(format!("{timestamp}_{suffix}.{extension}"));
        if !exists(&candidate) {
            return candidate;
        }
    }

    candidate
}

/// Writes a received screenshot, creating the `YYYY-MM` subfolder as needed. Returns its path.
pub fn store_screenshot(
    root: &Path,
    now: chrono::DateTime<chrono::Local>,
    screenshot: &CompletedScreenshot,
) -> Result<PathBuf> {
    let timestamp = now.format("%Y-%m-%d_%H-%M-%S").to_string();

    let path = build_screenshot_path(root, &timestamp, &screenshot.extension, &mut |path| {
        path.exists()
    });

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(&path, &screenshot.data)?;

    info!("Stored headset screenshot: {}", path.to_string_lossy());

    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn start(id: u32, extension: &str, chunk_count: u32, total_size: u64) -> ScreenshotStart {
        ScreenshotStart {
            id,
            extension: extension.into(),
            chunk_count,
            total_size,
        }
    }

    fn chunk(id: u32, chunk_index: u32, data: &[u8]) -> ScreenshotChunk {
        ScreenshotChunk {
            id,
            chunk_index,
            data: data.to_vec(),
        }
    }

    #[test]
    fn assembles_chunks_in_order() {
        let mut assembler = ScreenshotAssembler::new();
        assembler.handle_start(start(1, "jpeg", 3, 6));

        assert!(assembler.handle_chunk(chunk(1, 0, b"ab")).is_none());
        assert!(assembler.handle_chunk(chunk(1, 1, b"cd")).is_none());

        let done = assembler.handle_chunk(chunk(1, 2, b"ef")).unwrap();

        assert_eq!(done.data, b"abcdef");
        assert_eq!(done.extension, "jpeg");
        assert_eq!(assembler.pending_count(), 0);
    }

    #[test]
    fn reassembles_out_of_order_chunks() {
        let mut assembler = ScreenshotAssembler::new();
        assembler.handle_start(start(7, "png", 3, 6));

        assert!(assembler.handle_chunk(chunk(7, 2, b"ef")).is_none());
        assert!(assembler.handle_chunk(chunk(7, 0, b"ab")).is_none());

        let done = assembler.handle_chunk(chunk(7, 1, b"cd")).unwrap();

        assert_eq!(done.data, b"abcdef");
    }

    #[test]
    fn interleaves_two_transfers() {
        let mut assembler = ScreenshotAssembler::new();
        assembler.handle_start(start(1, "jpeg", 2, 4));
        assembler.handle_start(start(2, "png", 2, 4));

        assert!(assembler.handle_chunk(chunk(1, 0, b"aa")).is_none());
        assert!(assembler.handle_chunk(chunk(2, 0, b"bb")).is_none());
        assert!(assembler.handle_chunk(chunk(2, 1, b"bb")).is_some());

        let done = assembler.handle_chunk(chunk(1, 1, b"aa")).unwrap();

        assert_eq!(done.data, b"aaaa");
        assert_eq!(assembler.pending_count(), 0);
    }

    #[test]
    fn duplicate_chunk_does_not_complete_transfer() {
        let mut assembler = ScreenshotAssembler::new();
        assembler.handle_start(start(1, "jpeg", 2, 4));

        assert!(assembler.handle_chunk(chunk(1, 0, b"ab")).is_none());
        // Same index again: must not be mistaken for progress towards chunk_count.
        assert!(assembler.handle_chunk(chunk(1, 0, b"ab")).is_none());
        assert_eq!(assembler.pending_count(), 1);

        assert!(assembler.handle_chunk(chunk(1, 1, b"cd")).is_some());
    }

    #[test]
    fn chunk_without_start_is_ignored() {
        let mut assembler = ScreenshotAssembler::new();

        assert!(assembler.handle_chunk(chunk(9, 0, b"ab")).is_none());
        assert_eq!(assembler.pending_count(), 0);
    }

    #[test]
    fn out_of_range_index_drops_transfer() {
        let mut assembler = ScreenshotAssembler::new();
        assembler.handle_start(start(1, "jpeg", 2, 4));

        assert!(assembler.handle_chunk(chunk(1, 5, b"ab")).is_none());
        assert_eq!(assembler.pending_count(), 0);
    }

    #[test]
    fn oversized_payload_drops_transfer() {
        let mut assembler = ScreenshotAssembler::new();
        assembler.handle_start(start(1, "jpeg", 2, 4));

        assert!(assembler.handle_chunk(chunk(1, 0, b"aaaaaaaa")).is_none());
        assert_eq!(assembler.pending_count(), 0);
    }

    #[test]
    fn size_mismatch_on_last_chunk_is_rejected() {
        let mut assembler = ScreenshotAssembler::new();
        assembler.handle_start(start(1, "jpeg", 2, 8));

        assert!(assembler.handle_chunk(chunk(1, 0, b"ab")).is_none());
        // All chunks present but the announced total was never reached.
        assert!(assembler.handle_chunk(chunk(1, 1, b"cd")).is_none());
        assert_eq!(assembler.pending_count(), 0);
    }

    #[test]
    fn rejects_unsupported_extension() {
        let mut assembler = ScreenshotAssembler::new();
        assembler.handle_start(start(1, "mp4", 1, 2));

        assert_eq!(assembler.pending_count(), 0);
        assert!(assembler.handle_chunk(chunk(1, 0, b"ab")).is_none());
    }

    #[test]
    fn rejects_transfer_over_size_cap() {
        let mut assembler = ScreenshotAssembler::new();
        assembler.handle_start(start(1, "png", 1, SCREENSHOT_MAX_SIZE + 1));

        assert_eq!(assembler.pending_count(), 0);
    }

    #[test]
    fn rejects_empty_and_zero_chunk_transfers() {
        let mut assembler = ScreenshotAssembler::new();
        assembler.handle_start(start(1, "png", 1, 0));
        assembler.handle_start(start(2, "png", 0, 4));

        assert_eq!(assembler.pending_count(), 0);
    }

    #[test]
    fn extension_is_normalized_to_lowercase() {
        let mut assembler = ScreenshotAssembler::new();
        assembler.handle_start(start(1, "JPEG", 1, 2));

        let done = assembler.handle_chunk(chunk(1, 0, b"ab")).unwrap();

        assert_eq!(done.extension, "jpeg");
    }

    #[test]
    fn restarting_an_id_discards_the_stale_transfer() {
        let mut assembler = ScreenshotAssembler::new();
        assembler.handle_start(start(1, "jpeg", 2, 4));
        assert!(assembler.handle_chunk(chunk(1, 0, b"aa")).is_none());

        // Same id announced again: the half-received one must not leak into the new transfer.
        assembler.handle_start(start(1, "jpeg", 1, 2));

        let done = assembler.handle_chunk(chunk(1, 0, b"bb")).unwrap();

        assert_eq!(done.data, b"bb");
    }

    #[test]
    fn path_uses_month_subfolder_and_timestamp() {
        let root = PathBuf::from("root");

        let path = build_screenshot_path(&root, "2026-09-06_14-32-17", "jpeg", &mut |_| false);

        assert_eq!(path, root.join("2026-09").join("2026-09-06_14-32-17.jpeg"));
    }

    #[test]
    fn path_suffixes_on_collision() {
        let root = PathBuf::from("root");
        let taken: HashSet<PathBuf> = [
            root.join("2026-09").join("2026-09-06_14-32-17.jpeg"),
            root.join("2026-09").join("2026-09-06_14-32-17_1.jpeg"),
        ]
        .into_iter()
        .collect();

        let path = build_screenshot_path(&root, "2026-09-06_14-32-17", "jpeg", &mut |p| {
            taken.contains(p)
        });

        assert_eq!(
            path,
            root.join("2026-09").join("2026-09-06_14-32-17_2.jpeg")
        );
    }

    #[test]
    fn explicit_setting_overrides_pictures_dir() {
        let root = screenshots_root("  D:/shots  ").unwrap();

        assert_eq!(root, PathBuf::from("D:/shots"));
    }

    #[test]
    fn empty_setting_falls_back_to_pictures_alvr() {
        let root = screenshots_root("   ").unwrap();

        assert!(root.ends_with("ALVR"));
    }
}
