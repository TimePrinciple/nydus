// Copyright 2022 Nydus Developer. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

use std::array::IntoIter;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::prelude::*;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use nydus_utils::{digest::Algorithm, digest::RafsDigest, exec};
use vmm_sys_util::tempdir::TempDir;

use rafs::metadata::layout::RAFS_ROOT_INODE;
use rafs::metadata::{RafsInode, RafsMode, RafsSuper};
use rafs::RafsIoReader;
use storage::device::BlobChunkInfo;

fn create_dir(path: &Path) -> PathBuf {
    fs::create_dir_all(path).unwrap();
    path.to_owned()
}

fn create_file(path: &Path, chunks: &[Vec<u8>]) {
    let mut file = File::create(path).unwrap();
    for chunk in chunks {
        file.write_all(&chunk).unwrap();
    }
}

fn join_string(paths: Vec<&Path>, sep: &str) -> String {
    paths
        .iter()
        .map(|p| p.to_str().unwrap().to_string())
        .collect::<Vec<String>>()
        .join(sep)
}

fn overlay_mount(mut layer_paths: Vec<&Path>, target_dir: &Path) {
    layer_paths.reverse();
    exec(
        &format!(
            "mount -t overlay -o lowerdir={} overlay {}",
            join_string(layer_paths, ":"),
            target_dir.to_str().unwrap(),
        ),
        false,
    )
    .unwrap();
}

struct Skip {
    // for option --diff-skip-layer
    diff_skip_layer: usize,
    // for option --parent-bootstrap
    parent_bootstrap: PathBuf,
}

fn diff_build(
    work_dir: &Path,
    snapshot_paths: Vec<&Path>,
    layer_paths: Vec<&Path>,
    with_diff_hint: bool,
    chunk_dict_bootstrap: Option<&Path>,
    skip: Option<Skip>,
) {
    let builder = std::env::var("NYDUS_IMAGE")
        .unwrap_or_else(|_| String::from("./target-fusedev/release/nydus-image"));
    let output_path = work_dir.join("output.json");
    let bootstraps_path = create_dir(&work_dir.join("bootstraps"));
    let blobs_path = create_dir(&work_dir.join("blobs"));
    let cmd = format!(
        "
      {} create --log-level warn \
          --output-json {} \
          --compressor none \
          --chunk-size 0x1000 {} {} \
          --diff-bootstrap-dir {} \
          --blob-dir {} \
          --source-type diff {} {} {}
      ",
        builder,
        output_path.to_str().unwrap(),
        chunk_dict_bootstrap
            .map(|p| format!("--chunk-dict {}", p.to_str().unwrap()))
            .unwrap_or_default(),
        if let Some(skip) = skip {
            format!(
                "--diff-skip-layer {} --parent-bootstrap {}",
                skip.diff_skip_layer,
                skip.parent_bootstrap.to_str().unwrap(),
            )
        } else {
            String::new()
        },
        bootstraps_path.to_str().unwrap(),
        blobs_path.to_str().unwrap(),
        if with_diff_hint {
            "--diff-overlay-hint"
        } else {
            ""
        },
        join_string(snapshot_paths, " "),
        if with_diff_hint {
            join_string(layer_paths, " ")
        } else {
            String::new()
        },
    );
    exec(&cmd, false).unwrap();
}

fn generate_chunks(num: usize) -> (Vec<Vec<u8>>, Vec<String>) {
    let mut chunks = Vec::new();
    let mut digests = Vec::new();
    for _ in 0..num {
        let chunk = (0..0x1000)
            .map(|_| rand::random::<u8>())
            .collect::<Vec<u8>>();
        let digest = RafsDigest::from_buf(&chunk, Algorithm::Blake3);
        chunks.push(chunk);
        digests.push(format!("{}", digest));
    }
    (chunks, digests)
}

#[test]
fn integration_test_diff_build_with_chunk_dict() {
    let tmp_dir_prefix =
        std::env::var("TEST_WORKDIR_PREFIX").expect("Please specify `TEST_WORKDIR_PREFIX` env");
    let tmp_dir = TempDir::new_with_prefix(format!("{}/", tmp_dir_prefix)).unwrap();

    // ---------------------------------------------------
    // Test diff build with building chunk dict bootstrap

    // Create layer 1
    let layer_dir_1 = create_dir(&tmp_dir.as_path().join("layer-1"));
    let (layer_1_chunks_1, layer_1_chunk_digests_1) = generate_chunks(2);
    create_file(&layer_dir_1.join("file-1"), &layer_1_chunks_1);
    let (layer_1_chunks_2, layer_1_chunk_digests_2) = generate_chunks(1);
    create_file(&layer_dir_1.join("file-2"), &layer_1_chunks_2);
    // Create snapshot 1
    // Equals with layer-1, so nothing to do
    let snapshot_dir_1 = layer_dir_1.clone();

    // Create layer 2 (dump same blob with layer 1)
    let layer_dir_2 = create_dir(&tmp_dir.as_path().join("layer-2"));
    create_file(&layer_dir_2.join("file-3"), &layer_1_chunks_1);
    create_file(&layer_dir_2.join("file-4"), &layer_1_chunks_2);
    // Create snapshot 2
    let snapshot_dir_2 = create_dir(&tmp_dir.as_path().join("snapshot-2"));
    overlay_mount(vec![&layer_dir_1, &layer_dir_2], &snapshot_dir_2);

    // Create layer 3 (dump part of the same chunk with layer 1)
    let layer_dir_3 = create_dir(&tmp_dir.as_path().join("layer-3"));
    create_file(&layer_dir_3.join("file-5"), &[layer_1_chunks_1[1].clone()]);
    let (layer_3_chunks_1, layer_3_digests_1) = generate_chunks(1);
    create_file(&layer_dir_3.join("file-6"), &layer_3_chunks_1);
    // Create snapshot 3
    let snapshot_dir_3 = create_dir(&tmp_dir.as_path().join("snapshot-3"));
    overlay_mount(
        vec![&layer_dir_1, &layer_dir_2, &layer_dir_3],
        &snapshot_dir_3,
    );

    // Create layer 4 (dump empty blob)
    let layer_dir_4 = create_dir(&tmp_dir.as_path().join("layer-4"));
    create_file(&layer_dir_4.join("file-7"), &Vec::new());
    // Create snapshot 4
    let snapshot_dir_4 = create_dir(&tmp_dir.as_path().join("snapshot-4"));
    overlay_mount(
        vec![&layer_dir_1, &layer_dir_2, &layer_dir_3, &layer_dir_4],
        &snapshot_dir_4,
    );
    let expected_chunk_dict_bootstrap = IntoIter::new([
        (
            PathBuf::from("/file-1"),
            vec![
                (0, layer_1_chunk_digests_1[0].clone()),
                (0, layer_1_chunk_digests_1[1].clone()),
            ],
        ),
        (
            PathBuf::from("/file-2"),
            vec![(0, layer_1_chunk_digests_2[0].clone())],
        ),
        (
            PathBuf::from("/file-3"),
            vec![
                (1, layer_1_chunk_digests_1[0].clone()),
                (1, layer_1_chunk_digests_1[1].clone()),
            ],
        ),
        (
            PathBuf::from("/file-4"),
            vec![(1, layer_1_chunk_digests_2[0].clone())],
        ),
        (
            PathBuf::from("/file-5"),
            vec![(2, layer_1_chunk_digests_1[1].clone())],
        ),
        (
            PathBuf::from("/file-6"),
            vec![(2, layer_3_digests_1[0].clone())],
        ),
        (PathBuf::from("/file-7"), vec![]),
    ])
    .collect();

    // Diff build as a chunk-dict bootstrap
    let work_dir_1 = create_dir(&tmp_dir.as_path().join("workdir-1"));
    diff_build(
        &work_dir_1,
        vec![
            &snapshot_dir_1,
            &snapshot_dir_2,
            &snapshot_dir_3,
            &snapshot_dir_4,
        ],
        vec![&layer_dir_1, &layer_dir_2, &layer_dir_3, &layer_dir_4],
        true,
        None,
        None,
    );

    // Check metadata and chunks for chunk-dict bootstrap
    let file = OpenOptions::new()
        .read(true)
        .write(false)
        .open(work_dir_1.join("bootstraps/bootstrap-3"))
        .unwrap();
    let mut rs = RafsSuper {
        mode: RafsMode::Direct,
        validate_digest: true,
        ..Default::default()
    };
    let mut reader = Box::new(file) as RafsIoReader;
    rs.load(&mut reader).unwrap();
    let mut actual = HashMap::new();
    rs.walk_inodes(RAFS_ROOT_INODE, None, &mut |inode: &dyn RafsInode,
                                                path: &Path|
     -> Result<()> {
        let mut chunks = Vec::new();
        inode
            .walk_chunks(&mut |chunk: &dyn BlobChunkInfo| -> Result<()> {
                // let blobs = rs.superblock.get_blob_infos();
                chunks.push((chunk.blob_index(), format!("{}", chunk.chunk_id())));
                Ok(())
            })
            .unwrap();
        actual.insert(path.to_path_buf(), chunks);
        Ok(())
    })
    .unwrap();

    // Verify chunk-dict bootstrap
    assert_eq!(actual, expected_chunk_dict_bootstrap);

    // Print output.json
    let mut file = File::open(&work_dir_1.join("output.json")).unwrap();
    let mut contents = String::new();
    file.read_to_string(&mut contents).unwrap();

    // ---------------------------------------------------
    // Test diff build with chunk dict bootstrap

    // Create layer 5 (includes some chunks in chunk-dict)
    let layer_dir_5 = create_dir(&tmp_dir.as_path().join("layer-5"));
    create_file(
        &layer_dir_5.join("file-8"),
        &[layer_1_chunks_1[1].clone(), layer_1_chunks_2[0].clone()],
    );
    let (layer_5_chunks_1, layer_5_chunk_digests_1) = generate_chunks(2);
    create_file(&layer_dir_5.join("file-9"), &layer_5_chunks_1);
    // Create snapshot 5
    // Equals with layer-5, so nothing to do
    let snapshot_dir_5 = layer_dir_5.clone();

    // Create layer 6 (includes some chunks in chunk-dict)
    let layer_dir_6 = create_dir(&tmp_dir.as_path().join("layer-6"));
    let (layer_6_chunks_1, layer_6_chunk_digests_1) = generate_chunks(1);
    create_file(
        &layer_dir_6.join("file-10"),
        &[layer_6_chunks_1[0].clone(), layer_3_chunks_1[0].clone()],
    );
    // Create snapshot 6
    let snapshot_dir_6 = create_dir(&tmp_dir.as_path().join("snapshot-6"));
    overlay_mount(vec![&layer_dir_5, &layer_dir_6], &snapshot_dir_6);
    let expected_bootstrap = IntoIter::new([
        (
            PathBuf::from("/file-8"),
            vec![
                (1, layer_1_chunk_digests_1[1].clone()),
                (1, layer_1_chunk_digests_2[0].clone()),
            ],
        ),
        (
            PathBuf::from("/file-9"),
            vec![
                (0, layer_5_chunk_digests_1[0].clone()),
                (0, layer_5_chunk_digests_1[1].clone()),
            ],
        ),
        (
            PathBuf::from("/file-10"),
            vec![
                (2, layer_6_chunk_digests_1[0].clone()),
                (3, layer_3_digests_1[0].clone()),
            ],
        ),
    ])
    .collect();

    // Print output.json
    let mut file = File::open(&work_dir_1.join("output.json")).unwrap();
    let mut contents = String::new();
    file.read_to_string(&mut contents).unwrap();
    println!("DIFF_BUILD_WITH_CHUNK_DICT_OUTPUT {}", contents);

    // Diff build based on chunk-dict bootstrap
    let chunk_dict_bootstrap_path = &work_dir_1.join("bootstraps/bootstrap-3");
    let work_dir_2 = create_dir(&tmp_dir.as_path().join("workdir-2"));
    diff_build(
        &work_dir_2,
        vec![&snapshot_dir_5, &snapshot_dir_6],
        vec![&layer_dir_5, &layer_dir_6],
        true,
        Some(chunk_dict_bootstrap_path),
        None,
    );

    // Check metadata and chunks for final bootstrap
    let file = OpenOptions::new()
        .read(true)
        .write(false)
        .open(work_dir_2.join("bootstraps/bootstrap-1"))
        .unwrap();
    let mut rs = RafsSuper {
        mode: RafsMode::Direct,
        validate_digest: true,
        ..Default::default()
    };
    let mut reader = Box::new(file) as RafsIoReader;
    rs.load(&mut reader).unwrap();
    let mut actual = HashMap::new();
    rs.walk_inodes(RAFS_ROOT_INODE, None, &mut |inode: &dyn RafsInode,
                                                path: &Path|
     -> Result<()> {
        let mut chunks = Vec::new();
        inode
            .walk_chunks(&mut |chunk: &dyn BlobChunkInfo| -> Result<()> {
                chunks.push((chunk.blob_index(), format!("{}", chunk.chunk_id())));
                Ok(())
            })
            .unwrap();
        actual.insert(path.to_path_buf(), chunks);
        Ok(())
    })
    .unwrap();

    // Verify final bootstrap
    assert_eq!(actual, expected_bootstrap);

    // ---------------------------------------------------
    // Test diff build with build cache

    // Create layer 7
    let layer_dir_7 = create_dir(&tmp_dir.as_path().join("layer-7"));
    let (layer_7_chunks_1, layer_7_chunk_digests_1) = generate_chunks(2);
    create_file(&layer_dir_7.join("file-11"), &layer_7_chunks_1);

    // Create layer 8 (includes some chunks in chunk-dict)
    let layer_dir_8 = create_dir(&tmp_dir.as_path().join("layer-8"));
    let (layer_8_chunks_1, layer_8_chunk_digests_1) = generate_chunks(1);
    create_file(
        &layer_dir_8.join("file-12"),
        &[layer_8_chunks_1[0].clone(), layer_1_chunks_2[0].clone()],
    );

    // Create snapshot 7
    let snapshot_dir_7 = create_dir(&tmp_dir.as_path().join("snapshot-7"));
    overlay_mount(
        vec![
            &layer_dir_1,
            &layer_dir_2,
            &layer_dir_3,
            &layer_dir_4,
            &layer_dir_7,
        ],
        &snapshot_dir_7,
    );

    // Create snapshot 8
    let snapshot_dir_8 = create_dir(&tmp_dir.as_path().join("snapshot-8"));
    overlay_mount(
        vec![
            &layer_dir_1,
            &layer_dir_2,
            &layer_dir_3,
            &layer_dir_4,
            &layer_dir_7,
            &layer_dir_8,
        ],
        &snapshot_dir_8,
    );

    let expected_bootstrap = IntoIter::new([
        (
            PathBuf::from("/file-1"),
            vec![
                (0, layer_1_chunk_digests_1[0].clone()),
                (0, layer_1_chunk_digests_1[1].clone()),
            ],
        ),
        (
            PathBuf::from("/file-2"),
            vec![(0, layer_1_chunk_digests_2[0].clone())],
        ),
        (
            PathBuf::from("/file-3"),
            vec![
                (0, layer_1_chunk_digests_1[0].clone()),
                (0, layer_1_chunk_digests_1[1].clone()),
            ],
        ),
        (
            PathBuf::from("/file-4"),
            vec![(0, layer_1_chunk_digests_2[0].clone())],
        ),
        (
            PathBuf::from("/file-5"),
            vec![(0, layer_1_chunk_digests_1[1].clone())],
        ),
        (
            PathBuf::from("/file-6"),
            vec![(1, layer_3_digests_1[0].clone())],
        ),
        (PathBuf::from("/file-7"), vec![]),
        (
            PathBuf::from("/file-11"),
            vec![
                (2, layer_7_chunk_digests_1[0].clone()),
                (2, layer_7_chunk_digests_1[1].clone()),
            ],
        ),
        (
            PathBuf::from("/file-12"),
            vec![
                (3, layer_8_chunk_digests_1[0].clone()),
                (0, layer_1_chunk_digests_2[0].clone()),
            ],
        ),
    ])
    .collect();

    // Diff build based on chunk-dict bootstrap
    let chunk_dict_bootstrap_path = &work_dir_1.join("bootstraps/bootstrap-3");
    let work_dir_3 = create_dir(&tmp_dir.as_path().join("workdir-3"));
    diff_build(
        &work_dir_3,
        vec![
            &snapshot_dir_1,
            &snapshot_dir_2,
            &snapshot_dir_3,
            &snapshot_dir_4,
            &snapshot_dir_7,
            &snapshot_dir_8,
        ],
        vec![
            &layer_dir_1,
            &layer_dir_2,
            &layer_dir_3,
            &layer_dir_4,
            &layer_dir_7,
            &layer_dir_8,
        ],
        true,
        Some(chunk_dict_bootstrap_path),
        Some(Skip {
            diff_skip_layer: 3,
            parent_bootstrap: work_dir_1.join("bootstraps/bootstrap-3"),
        }),
    );

    // Check metadata and chunks for final bootstrap
    let file = OpenOptions::new()
        .read(true)
        .write(false)
        .open(work_dir_3.join("bootstraps/bootstrap-5"))
        .unwrap();
    let mut rs = RafsSuper {
        mode: RafsMode::Direct,
        validate_digest: true,
        ..Default::default()
    };
    let mut reader = Box::new(file) as RafsIoReader;
    rs.load(&mut reader).unwrap();
    let mut actual = HashMap::new();
    rs.walk_inodes(RAFS_ROOT_INODE, None, &mut |inode: &dyn RafsInode,
                                                path: &Path|
     -> Result<()> {
        let mut chunks = Vec::new();
        inode
            .walk_chunks(&mut |chunk: &dyn BlobChunkInfo| -> Result<()> {
                chunks.push((chunk.blob_index(), format!("{}", chunk.chunk_id())));
                Ok(())
            })
            .unwrap();
        actual.insert(path.to_path_buf(), chunks);
        Ok(())
    })
    .unwrap();

    assert_eq!(actual, expected_bootstrap);
}
