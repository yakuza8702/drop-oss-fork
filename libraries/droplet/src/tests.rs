#![cfg(test)]
extern crate test_generator;

use std::path::Path;

use test_generator::test_resources;
use tokio::io::SimplexStream;

use crate::manifest::{generate_manifest_rusty, ManifestWriterFactory};

#[test_resources("testfiles/**/*.7z")]
fn manifest_gen(resource: &str) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime");

    runtime.block_on(async move {
        let filepath = Path::new(resource);
        let manifest = generate_manifest_rusty(
            filepath,
            |_| {},
            |message| {
                println!("({}) {}", filepath.display(), message);
            },
            None::<&dyn ManifestWriterFactory<Writer = SimplexStream>>, // Dummy type signature, not actually used
            None,
        )
        .await
        .unwrap_or_else(|err| {
            panic!(
                "failed to generate manifest for {}: {:?}",
                filepath.display(),
                err
            )
        });

        let first_chunk = manifest
            .chunks
            .values()
            .next()
            .expect("no chunks generated");
        let first_chunk_length = first_chunk.files.len();
        if first_chunk_length == 0 {
            panic!("{} has no files in manifest", filepath.display());
        }
    });
}

// ---------------------------------------------------------------------------
// A/B compatibility tests for archive-backed versions
// ---------------------------------------------------------------------------
//
// Drives the real backend through the same code path the manifest generator and
// the depot server use, with a deliberately small (4 KiB) read buffer - the size
// hyper hands to the reader when it serves a chunk.
//
//   * pristine v0.4.0-rc-6  -> expected to fail (archive reader ignores ranges)
//   * patched (7z backend)  -> expected to pass byte-for-byte
#[cfg(test)]
mod archive_compat {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use tokio::io::AsyncReadExt as _;

    use crate::versions::{
        create_backend_constructor,
        types::{VersionBackend, VersionFile},
    };

    const CANDIDATES: [&str; 6] = ["7z", "7zz", "7za", "7zr", "7z.exe", "7za.exe"];

    fn sevenzip() -> PathBuf {
        if let Ok(explicit) = std::env::var("SEVENZIP_PATH") {
            if !explicit.is_empty() {
                return PathBuf::from(explicit);
            }
        }
        if let Some(paths) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&paths) {
                for candidate in CANDIDATES {
                    let full = dir.join(candidate);
                    if full.is_file() {
                        return full;
                    }
                }
            }
        }
        panic!("no 7z binary on PATH - the test needs one to build fixtures");
    }

    /// Deterministic pseudo-random bytes: compressible enough to be realistic,
    /// not so compressible that everything lands in one block.
    fn lcg(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        (0..len)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) as u8
            })
            .collect()
    }

    struct Fixture {
        root: PathBuf,
        archive: PathBuf,
        expected: Vec<(String, Vec<u8>)>,
    }

    fn fixture(label: &str, pack_args: &[&str]) -> Fixture {
        let root = std::env::temp_dir().join(format!(
            "droplet-archive-{}-{}",
            label,
            uuid::Uuid::new_v4()
        ));
        let src = root.join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();

        let expected: Vec<(String, Vec<u8>)> = vec![
            ("readme.txt".to_string(), b"hello drop\n".to_vec()),
            ("big.bin".to_string(), lcg(1, 3 * 1024 * 1024)),
            ("mid.dat".to_string(), lcg(2, 200 * 1024)),
            ("sp ace-\u{fc}n\u{ef}code.bin".to_string(), lcg(3, 40 * 1024)),
            ("sub/nested.bin".to_string(), lcg(4, 12 * 1024)),
        ];

        for (name, bytes) in &expected {
            let path = src.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, bytes).unwrap();
        }

        let archive = root.join("version.7z");
        let status = Command::new(sevenzip())
            .current_dir(&src)
            .args(["a", "-t7z", "-mx=9", "-bso0", "-bsp0", "-mmt=2"])
            .args(pack_args)
            .arg(&archive)
            .args(expected.iter().map(|(name, _)| name))
            .status()
            .expect("failed to run 7z");
        assert!(status.success(), "[{label}] 7z failed to create the archive");

        Fixture {
            root,
            archive,
            expected,
        }
    }

    fn backend_for(archive: &Path) -> Box<dyn VersionBackend + Send + Sync> {
        let ctor = create_backend_constructor(archive)
            .expect("archive extension is not recognised by create_backend_constructor");
        ctor().expect("failed to construct the backend")
    }

    async fn read_range(
        backend: &(dyn VersionBackend + Send + Sync),
        file: &VersionFile,
        start: u64,
        end: u64,
    ) -> Vec<u8> {
        let mut reader = backend.reader(file, start, end).await.expect("reader() failed");
        // 4 KiB: the buffer size the download server uses. A reader that ignores
        // the caller's remaining capacity blows up here.
        let mut buf = vec![0u8; 4096];
        let mut out = Vec::new();
        loop {
            let amount = reader.read(&mut buf).await.expect("read() failed");
            if amount == 0 {
                break;
            }
            out.extend_from_slice(&buf[..amount]);
        }
        out
    }

    #[tokio::test]
    async fn archive_backend_reads_exact_bytes() {
        for (label, pack_args) in [
            (
                "non-solid-256k",
                &["-ms=off", "-md=256k", "-mfb=64"][..],
            ),
            ("solid-16g-64m", &["-ms=16g", "-md=64m", "-mfb=64"][..]),
        ] {
            let fx = fixture(label, pack_args);
            let backend = backend_for(&fx.archive);

            let files = backend
                .list_files()
                .await
                .unwrap_or_else(|err| panic!("[{label}] list_files failed: {err:?}"));

            let mut got: Vec<(String, u64)> = files
                .iter()
                .map(|f| (f.relative_filename.clone(), f.size))
                .collect();
            got.sort();

            let mut want: Vec<(String, u64)> = fx
                .expected
                .iter()
                .map(|(name, bytes)| (name.clone(), bytes.len() as u64))
                .collect();
            want.sort();

            assert_eq!(got, want, "[{label}] file list / sizes differ from the source tree");

            for (name, bytes) in &fx.expected {
                let file = files
                    .iter()
                    .find(|f| &f.relative_filename == name)
                    .unwrap_or_else(|| panic!("[{label}] {name} missing from list_files"));

                let whole = read_range(backend.as_ref(), file, 0, 0).await;
                assert_eq!(
                    whole.len(),
                    bytes.len(),
                    "[{label}] {name}: whole-file length differs"
                );
                assert!(
                    whole == *bytes,
                    "[{label}] {name}: whole-file content differs"
                );

                if bytes.len() > 8192 {
                    // A range that starts and ends in the middle of a 4 KiB block.
                    let start = 1234u64;
                    let end = start + 5000;
                    let part = read_range(backend.as_ref(), file, start, end).await;
                    assert_eq!(
                        part.len(),
                        (end - start) as usize,
                        "[{label}] {name}: ranged read length differs"
                    );
                    assert_eq!(
                        part,
                        bytes[start as usize..end as usize].to_vec(),
                        "[{label}] {name}: ranged read content differs"
                    );
                }
            }

            std::fs::remove_dir_all(&fx.root).ok();
        }
    }

    #[tokio::test]
    async fn archive_manifest_reports_uncompressed_size() {
        let fx = fixture("manifest", &["-ms=off", "-md=256k"]);
        let manifest = crate::manifest::generate_manifest_rusty(
            &fx.archive,
            |_| {},
            |message| println!("(manifest) {message}"),
            None::<&dyn crate::manifest::ManifestWriterFactory<Writer = tokio::io::SimplexStream>>,
            None,
        )
        .await
        .expect("manifest generation failed");

        let want: u64 = fx.expected.iter().map(|(_, bytes)| bytes.len() as u64).sum();
        assert_eq!(
            manifest.size, want,
            "manifest size must be the uncompressed total, not the archive size"
        );

        // Every file must be covered exactly once, and nothing may be declared
        // beyond the real size of the file.
        for (name, bytes) in &fx.expected {
            let covered: u64 = manifest
                .chunks
                .values()
                .flat_map(|chunk| chunk.files.iter())
                .filter(|entry| &entry.filename == name)
                .map(|entry| entry.length as u64)
                .sum();
            assert_eq!(
                covered,
                bytes.len() as u64,
                "manifest coverage for {name} is wrong"
            );
        }

        std::fs::remove_dir_all(&fx.root).ok();
    }
}

// ---------------------------------------------------------------------------
// Which entries make the pristine (libarchive) lister stop early?
// Prints the observed listing for each variant so the A/B output shows it.
#[cfg(test)]
mod archive_listing_probe {
    use std::path::PathBuf;
    use std::process::Command;

    use crate::versions::{create_backend_constructor, types::VersionBackend};

    fn sevenzip() -> PathBuf {
        for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap()) {
            for candidate in ["7z", "7zz", "7za", "7zr"] {
                let full = dir.join(candidate);
                if full.is_file() {
                    return full;
                }
            }
        }
        panic!("no 7z");
    }

    fn probe(label: &str, names: &[&str]) {
        let root = std::env::temp_dir().join(format!("probe-{}-{}", label, uuid::Uuid::new_v4()));
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        for name in names {
            let path = src.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"payload-1234").unwrap();
        }
        let archive = root.join("v.7z");
        let status = Command::new(sevenzip())
            .current_dir(&src)
            .args(["a", "-t7z", "-bso0", "-bsp0", "-ms=off"])
            .arg(&archive)
            .args(names)
            .status()
            .unwrap();
        assert!(status.success());

        let backend = create_backend_constructor(&archive).unwrap()();
        let backend = backend.unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let listed = runtime.block_on(async { backend.list_files().await.unwrap() });
        let mut got: Vec<String> = listed.into_iter().map(|f| f.relative_filename).collect();
        got.sort();
        let mut want: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        want.sort();
        println!("PROBE {label}: want={want:?} got={got:?}");
        assert_eq!(got, want, "PROBE MISMATCH {label}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn name_variants() {
        probe("plain", &["plain.txt", "second.txt"]);
        probe("space", &["has space.txt", "second.txt"]);
        probe("unicode", &["\u{fc}n\u{ef}code.txt", "second.txt"]);
        probe("nested", &["sub/nested.txt", "second.txt"]);
        probe("space+unicode", &["sp ace-\u{fc}n\u{ef}code.txt", "second.txt"]);
    }
}
