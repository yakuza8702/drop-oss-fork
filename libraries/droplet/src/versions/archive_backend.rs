//! Archive-backed versions, read through the real `7z` binary.
//!
//! This is a drop-in replacement for Drop's libarchive-based archive backend
//! (`libraries/droplet/src/versions/archive_backend.rs`). It restores the
//! 0.3.x behaviour: instead of parsing the archive in-process with libarchive,
//! it shells out to the `7z` binary that the Drop image already installs
//! (`apk add --no-cache pnpm 7zip nginx`).
//!
//! Why: the libarchive reader ignored the requested byte range
//! (`reader(file, _start, _end)`) and could hand out data from the wrong offset,
//! which made clients download short/partial files from archive versions.
//! `7z x -so` gives a byte-exact stream for one entry, and the reader below
//! honours `start`/`end` (and never writes more than fits in the caller's buffer).
//!
//! Behaviour kept from the original backend:
//!   * `require_whole_files()` stays `true` - random access into a 7z entry
//!     means decompressing from the start of its block anyway, so Drop keeps
//!     whole files inside a single chunk.
//!   * `list_files()` reports the uncompressed size of every regular file.

use std::{
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    task::{Context, Poll},
};

use anyhow::{anyhow, bail, Context as _};
use async_trait::async_trait;
use tokio::{
    io::{AsyncRead, ReadBuf},
    process::{Child, ChildStdout, Command},
};

use crate::versions::types::{MinimumFileObject, VersionBackend, VersionFile};

/// Binaries we accept as "7z", in order of preference. Alpine's `7zip` package
/// installs `7z` (a symlink to `7zz`); `7za`/`7zr` come from 7-Zip Extra.
const SEVENZIP_CANDIDATES: [&str; 6] = ["7z", "7zz", "7za", "7zr", "7z.exe", "7za.exe"];

/// Set `SEVENZIP_PATH` to force a specific binary.
fn find_sevenzip() -> anyhow::Result<PathBuf> {
    if let Ok(explicit) = std::env::var("SEVENZIP_PATH") {
        if !explicit.is_empty() {
            return Ok(PathBuf::from(explicit));
        }
    }

    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            for candidate in SEVENZIP_CANDIDATES {
                let full = dir.join(candidate);
                if full.is_file() {
                    return Ok(full);
                }
            }
        }
    }

    Err(anyhow!(
        "no 7z binary found on PATH (looked for {:?}); install 7zip or set SEVENZIP_PATH",
        SEVENZIP_CANDIDATES
    ))
}

pub struct ZipVersionBackend {
    path: PathBuf,
    sevenzip: PathBuf,
}

impl ZipVersionBackend {
    pub fn new(path: PathBuf) -> anyhow::Result<Self> {
        let sevenzip = find_sevenzip()?;
        Ok(Self { path, sevenzip })
    }

    /// `7z l -slt` prints one `key = value` per line, blocks separated by a
    /// blank line: first the archive itself, then every entry. Entries always
    /// carry an `Attributes` line, the archive block does not.
    async fn list_entries(&self) -> anyhow::Result<Vec<VersionFile>> {
        let output = Command::new(&self.sevenzip)
            .arg("l")
            .arg("-slt")
            .arg("-sccUTF-8")
            .arg("--")
            .arg(&self.path)
            .stdin(Stdio::null())
            .output()
            .await
            .with_context(|| format!("failed to execute {}", self.sevenzip.display()))?;

        if !output.status.success() {
            bail!(
                "7z could not list {}: {}",
                self.path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let listing = String::from_utf8(output.stdout)
            .with_context(|| format!("7z produced non-UTF-8 output for {}", self.path.display()))?;

        let mut files = Vec::new();
        let mut path: Option<String> = None;
        let mut size: u64 = 0;
        let mut attributes: Option<String> = None;

        // An extra empty line flushes the final block.
        for line in listing.lines().chain(std::iter::once("")) {
            if line.trim().is_empty() {
                if let (Some(relative_filename), Some(attributes)) =
                    (path.take(), attributes.take())
                {
                    let is_dir = attributes.starts_with('D') || relative_filename.ends_with('/');
                    if !is_dir {
                        files.push(VersionFile {
                            relative_filename,
                            permission: 0o744,
                            size,
                        });
                    }
                }
                size = 0;
                continue;
            }

            if let Some(value) = line.strip_prefix("Path = ") {
                path = Some(value.to_string());
            } else if let Some(value) = line.strip_prefix("Size = ") {
                size = value.trim().parse().unwrap_or(0);
            } else if let Some(value) = line.strip_prefix("Attributes = ") {
                attributes = Some(value.to_string());
            }
        }

        Ok(files)
    }
}

/// Streams one archive entry from `7z x -so`, skipping `start` bytes and
/// stopping after `end` (exclusive; `end == 0` means "to the end of the file").
struct ArchiveEntryReader {
    child: Child,
    stdout: ChildStdout,
    name: String,
    skip: u64,
    remaining: Option<u64>,
    scratch: Vec<u8>,
    done: bool,
}

impl ArchiveEntryReader {
    fn new(child: Child, stdout: ChildStdout, name: String, start: u64, end: u64) -> Self {
        Self {
            child,
            stdout,
            name,
            skip: start,
            remaining: if end == 0 {
                None
            } else {
                Some(end.saturating_sub(start))
            },
            scratch: vec![0u8; 64 * 1024],
            done: false,
        }
    }
}

impl AsyncRead for ArchiveEntryReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let me = self.get_mut();

        if me.done || buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }

        // Discard everything before `start`.
        while me.skip > 0 {
            let want = me.skip.min(me.scratch.len() as u64) as usize;
            let read = {
                let mut scratch_buf = ReadBuf::new(&mut me.scratch[..want]);
                match Pin::new(&mut me.stdout).poll_read(cx, &mut scratch_buf) {
                    Poll::Ready(Ok(())) => scratch_buf.filled().len(),
                    Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                    Poll::Pending => return Poll::Pending,
                }
            };

            if read == 0 {
                me.done = true;
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!(
                        "archive entry `{}` ended before offset {}",
                        me.name, me.skip
                    ),
                )));
            }

            me.skip -= read as u64;
        }

        let want = match me.remaining {
            Some(0) => {
                me.done = true;
                return Poll::Ready(Ok(()));
            }
            Some(remaining) => (remaining).min(buf.remaining() as u64) as usize,
            None => buf.remaining(),
        };
        let want = want.min(me.scratch.len()).max(1);

        let read = {
            let mut scratch_buf = ReadBuf::new(&mut me.scratch[..want]);
            match Pin::new(&mut me.stdout).poll_read(cx, &mut scratch_buf) {
                Poll::Ready(Ok(())) => scratch_buf.filled().len(),
                Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                Poll::Pending => return Poll::Pending,
            }
        };

        if read == 0 {
            me.done = true;
            if me.remaining.is_some_and(|remaining| remaining > 0) {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!("archive entry `{}` ended early", me.name),
                )));
            }
            return Poll::Ready(Ok(()));
        }

        // Never write more than the caller's buffer can hold.
        let take = read.min(buf.remaining());
        buf.put_slice(&me.scratch[..take]);

        if let Some(remaining) = me.remaining.as_mut() {
            *remaining -= take as u64;
        }

        Poll::Ready(Ok(()))
    }
}

impl Drop for ArchiveEntryReader {
    fn drop(&mut self) {
        // The consumer may abandon a chunk half way through; do not leak 7z.
        let _ = self.child.start_kill();
        let _ = self.child.try_wait();
    }
}

#[async_trait]
impl VersionBackend for ZipVersionBackend {
    async fn list_files(&self) -> anyhow::Result<Vec<VersionFile>> {
        self.list_entries().await
    }

    async fn reader(
        &self,
        file: &VersionFile,
        start: u64,
        end: u64,
    ) -> anyhow::Result<Box<dyn MinimumFileObject>> {
        let mut child = Command::new(&self.sevenzip)
            .arg("x")
            .arg("-so") // write the extracted data to stdout
            .arg("-y") // assume yes on all prompts
            .arg("-bd") // no progress indicator
            .arg("-sccUTF-8")
            .arg("--")
            .arg(&self.path)
            .arg("--")
            .arg(&file.relative_filename)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to execute {}", self.sevenzip.display()))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to capture 7z stdout"))?;

        Ok(Box::new(ArchiveEntryReader::new(
            child,
            stdout,
            file.relative_filename.clone(),
            start,
            end,
        )))
    }

    async fn peek_file(&self, sub_path: String) -> anyhow::Result<VersionFile> {
        let files = self.list_files().await?;
        files
            .into_iter()
            .find(|v| v.relative_filename == sub_path)
            .ok_or_else(|| anyhow!("entry not found: {}", sub_path))
    }

    fn require_whole_files(&self) -> bool {
        true
    }
}

#[allow(dead_code)]
fn _assert_path_is_used(_: &Path) {}
