// Copyright 2020 Ant Group. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

//! File node for RAFS format

use nix::sys::stat;
use rafs::RafsIoWriter;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File};
use std::io::prelude::*;
use std::io::{Error, Result};
use std::os::linux::fs::MetadataExt;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::str;
use std::str::FromStr;

use sha2::digest::Digest;
use sha2::Sha256;

use nydus_utils::div_round_up;
use nydus_utils::{einval, last_error};

use rafs::metadata::digest::{self, RafsDigest};
use rafs::metadata::layout::*;
use rafs::metadata::*;
use rafs::storage::compress;

const ROOT_PATH_NAME: &[u8] = &[b'/'];

pub const OCISPEC_WHITEOUT_PREFIX: &str = ".wh.";
pub const OCISPEC_WHITEOUT_OPAQUE: &str = ".wh..wh..opq";

#[derive(Clone, Debug, PartialEq)]
pub enum WhiteoutType {
    OCIOpaque,
    OCIRemoval,
    OverlayFSOpaque,
    OverlayFSRemoval,
}

impl WhiteoutType {
    pub fn is_removal(&self) -> bool {
        *self == WhiteoutType::OCIRemoval || *self == WhiteoutType::OverlayFSRemoval
    }
}

#[derive(PartialEq)]
pub enum WhiteoutSpec {
    /// https://github.com/opencontainers/image-spec/blob/master/layer.md#whiteouts
    Oci,
    /// "whiteouts and opaque directories" in https://www.kernel.org/doc/Documentation/filesystems/overlayfs.txt
    Overlayfs,
}

impl FromStr for WhiteoutSpec {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "oci" => Ok(Self::Oci),
            "overlayfs" => Ok(Self::Overlayfs),
            _ => Err(einval!("Invalid whiteout spec")),
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq)]
pub enum Overlay {
    Lower,
    UpperAddition,
    UpperOpaque,
    UpperRemoval,
    UpperModification,
}

impl Overlay {
    pub fn lower_layer(&self) -> bool {
        self == &Overlay::Lower
    }
}

impl fmt::Display for Overlay {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Overlay::Lower => write!(f, "LOWER"),
            Overlay::UpperAddition => write!(f, "ADDED"),
            Overlay::UpperOpaque => write!(f, "OPAQUED"),
            Overlay::UpperRemoval => write!(f, "REMOVED"),
            Overlay::UpperModification => write!(f, "MODIFIED"),
        }
    }
}

impl fmt::Display for Node {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{} {:?}: index {} ino {} real_ino {} i_parent {} child_index {} child_count {} i_nlink {} i_size {} i_name_size {} i_symlink_size {} has_xattr {} link {:?}",
            self.file_type(),
            self.rootfs(),
            self.index,
            self.inode.i_ino,
            self.real_ino,
            self.inode.i_parent,
            self.inode.i_child_index,
            self.inode.i_child_count,
            self.inode.i_nlink,
            self.inode.i_size,
            self.inode.i_name_size,
            self.inode.i_symlink_size,
            self.inode.has_xattr(),
            self.symlink,
        )
    }
}

#[derive(Clone)]
pub struct Node {
    pub index: u64,
    /// Inode number in local filesystem
    pub real_ino: Inode,
    /// dev number is required because a source root directory can have multiple
    /// partitions mounted. Files from different partition can have unique inode number.
    pub dev: u64,
    /// device ID (if special file), describes the device that this file (inode) represents.
    pub rdev: u64,
    /// Overlay type for layered build
    pub overlay: Overlay,
    /// Absolute path to root directory where start to build image.
    /// For example: /home/source
    pub source: PathBuf,
    /// Absolute path to each file within build context directory.
    /// Together with `source`, we can easily get relative path to `source`.
    /// For example: /home/source/foo/bar
    pub path: PathBuf,
    /// Define a disk inode structure to persist to disk.
    pub inode: OndiskInode,
    /// Chunks info list of regular file
    pub chunks: Vec<OndiskChunkInfo>,
    /// Symlink info of symlink file
    pub symlink: Option<OsString>,
    /// Xattr list of file
    pub xattrs: XAttrs,
    pub explicit_uidgid: bool,
}

impl Node {
    pub fn new(
        source: PathBuf,
        path: PathBuf,
        overlay: Overlay,
        explicit_uidgid: bool,
    ) -> Result<Node> {
        let mut node = Node {
            index: 0,
            real_ino: 0,
            dev: u64::MAX,
            rdev: u64::MAX,
            source,
            path,
            overlay,
            inode: OndiskInode::new(),
            chunks: Vec::new(),
            symlink: None,
            xattrs: XAttrs::default(),
            explicit_uidgid,
        };
        node.build_inode()?;
        Ok(node)
    }

    fn build_inode_xattr(&mut self) -> Result<()> {
        let file_xattrs = xattr::list(&self.path)?;

        for key in file_xattrs {
            let value = xattr::get(&self.path, &key)?;
            self.xattrs.pairs.insert(key, value.unwrap_or_default());
        }

        if !self.xattrs.pairs.is_empty() {
            self.inode.i_flags |= RafsInodeFlags::XATTR;
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn dump_blob(
        &mut self,
        f_blob: &mut RafsIoWriter,
        blob_hash: &mut Sha256,
        compress_offset: &mut u64,
        decompress_offset: &mut u64,
        chunk_cache: &mut HashMap<RafsDigest, OndiskChunkInfo>,
        compressor: compress::Algorithm,
        digester: digest::Algorithm,
        blob_index: u32,
    ) -> Result<usize> {
        if self.is_dir() {
            return Ok(0);
        }

        if self.is_symlink() {
            self.inode.i_digest =
                RafsDigest::from_buf(self.symlink.as_ref().unwrap().as_bytes(), digester);
            return Ok(0);
        }

        let file_size = self.inode.i_size;
        let mut blob_size = 0usize;
        let mut inode_hasher = RafsDigest::hasher(digester);
        let mut file = File::open(&self.path).map_err(|e| last_error!(e))?;

        for i in 0..self.inode.i_child_count {
            // Init chunk info
            let mut chunk = OndiskChunkInfo::new();
            // FIXME: Should not assume that block size must be the default one.
            // Use the configured value instead!
            let file_offset = i as u64 * RAFS_DEFAULT_BLOCK_SIZE;
            let chunk_size = if i == self.inode.i_child_count - 1 {
                file_size as usize - (RAFS_DEFAULT_BLOCK_SIZE as usize * i as usize)
            } else {
                RAFS_DEFAULT_BLOCK_SIZE as usize
            };

            // Read chunk data
            let mut chunk_data = vec![0; chunk_size];
            file.read_exact(&mut chunk_data)?;

            // Calculate chunk digest
            // TODO: check for hole chunks. One possible way is to always save
            // a global hole chunk and check for digest duplication
            chunk.block_id = RafsDigest::from_buf(chunk_data.as_slice(), digester);
            // Calculate inode digest
            inode_hasher.digest_update(chunk.block_id.as_ref());

            // Deduplicate chunk if we found a same one from chunk cache
            if let Some(cached_chunk) = chunk_cache.get(&chunk.block_id) {
                // hole cached_chunk can have zero decompress size
                if cached_chunk.decompress_size == 0
                    || cached_chunk.decompress_size == chunk_size as u32
                {
                    chunk.clone_from(&cached_chunk);
                    chunk.file_offset = file_offset;
                    self.chunks.push(chunk);
                    trace!(
                        "\t\tbuilding duplicated chunk: {} compressor {}",
                        chunk,
                        compressor,
                    );
                    continue;
                }
            }

            // Compress chunk data
            let (compressed, is_compressed) = compress::compress(&chunk_data, compressor)?;
            let compressed_size = compressed.len();
            if is_compressed {
                chunk.flags |= RafsChunkFlags::COMPRESSED;
            }

            chunk.blob_index = blob_index;
            chunk.file_offset = file_offset;
            chunk.compress_offset = *compress_offset;
            chunk.decompress_offset = *decompress_offset;
            chunk.compress_size = compressed_size as u32;
            chunk.decompress_size = chunk_size as u32;
            blob_size += compressed_size;

            // Move cursor to offset of next chunk
            *compress_offset += compressed_size as u64;
            *decompress_offset += chunk_size as u64;

            // Calculate blob hash
            blob_hash.update(&compressed);

            // Dump compressed chunk data to blob
            f_blob.write_all(&compressed)?;

            // Cache chunk digest info
            chunk_cache.insert(chunk.block_id, chunk);
            self.chunks.push(chunk);

            trace!("\t\tbuilding chunk: {} compressor {}", chunk, compressor,);
        }

        // Finish inode digest calculation
        self.inode.i_digest = inode_hasher.digest_finalize();

        Ok(blob_size)
    }

    pub fn dump_bootstrap(&mut self, f_bootstrap: &mut RafsIoWriter) -> Result<usize> {
        let mut node_size = 0;

        // Dump inode info
        let name = self.name();
        let inode = OndiskInodeWrapper {
            name,
            symlink: self.symlink.as_deref(),
            inode: &self.inode,
        };
        let inode_size = inode.store(f_bootstrap)?;
        node_size += inode_size;

        // Dump inode xattr
        if !self.xattrs.pairs.is_empty() {
            let xattr_size = self.xattrs.store(f_bootstrap)?;
            node_size += xattr_size;
        }

        // Dump chunk info
        if self.is_reg() && self.inode.i_child_count as usize != self.chunks.len() {
            return Err(einval!(format!(
                "invalid chunks count {}: {}",
                self.chunks.len(),
                self
            )));
        }

        for chunk in &mut self.chunks {
            let chunk_size = chunk.store(f_bootstrap)?;
            node_size += chunk_size;
        }

        Ok(node_size)
    }

    fn build_inode_stat(&mut self) -> Result<()> {
        let meta = self.meta()?;

        self.inode.i_mode = meta.st_mode();
        if self.explicit_uidgid {
            self.inode.i_uid = meta.st_uid();
            self.inode.i_gid = meta.st_gid();
        }
        self.inode.i_projid = 0;
        self.inode.i_size = meta.st_size();
        // Ignore actual nlink value and calculate from rootfs directory instead
        self.inode.i_nlink = 1;
        self.inode.i_blocks = meta.st_blocks();
        self.inode.i_blocks = div_round_up(self.inode.i_size, 512);

        self.real_ino = meta.st_ino();
        self.dev = meta.st_dev();
        self.rdev = meta.st_rdev();

        Ok(())
    }

    fn build_inode(&mut self) -> Result<()> {
        self.inode.set_name_size(self.name().as_bytes().len());
        self.build_inode_stat()?;

        if self.is_reg() {
            self.inode.i_child_count = self.chunk_count() as u32;
        } else if self.is_symlink() {
            self.inode.i_flags |= RafsInodeFlags::SYMLINK;
            let target_path = fs::read_link(&self.path)?;
            self.symlink = Some(target_path.into());
            self.inode
                .set_symlink_size(self.symlink.as_ref().unwrap().as_bytes().len());
        }

        self.build_inode_xattr()?;

        Ok(())
    }

    pub fn meta(&self) -> Result<impl MetadataExt> {
        self.path.symlink_metadata().map_err(|e| einval!(e))
    }

    /// Generate the path relative to original rootfs.
    /// For example:
    /// `/absolute/path/to/rootfs/file` after converting `/file`
    pub fn rootfs(&self) -> PathBuf {
        if let Ok(rootfs) = self.path.strip_prefix(&self.source) {
            Path::new("/").join(rootfs)
        } else {
            // Compatible with path `/`
            self.path.clone()
        }
    }

    pub fn is_dir(&self) -> bool {
        self.inode.i_mode & libc::S_IFMT == libc::S_IFDIR
    }

    pub fn is_symlink(&self) -> bool {
        self.inode.i_mode & libc::S_IFMT == libc::S_IFLNK
    }

    pub fn is_reg(&self) -> bool {
        self.inode.i_mode & libc::S_IFMT == libc::S_IFREG
    }

    pub fn is_hardlink(&self) -> bool {
        self.inode.i_nlink > 1
    }

    pub fn chunk_count(&self) -> usize {
        if !self.is_reg() {
            return 0;
        }
        div_round_up(self.inode.i_size, RAFS_DEFAULT_BLOCK_SIZE) as usize
    }

    pub fn file_type(&self) -> &str {
        let mut file_type = "";

        if self.is_symlink() {
            file_type = "symlink";
        } else if self.is_dir() {
            file_type = "dir"
        } else if self.is_reg() {
            if self.is_hardlink() {
                file_type = "hardlink";
            } else {
                file_type = "file";
            }
        }

        file_type
    }

    pub fn name(&self) -> &OsStr {
        if self.path == self.source {
            OsStr::from_bytes(ROOT_PATH_NAME)
        } else {
            // Safe to unwrap because `path` should be returned from `path()` which is canonicalized
            self.path.file_name().unwrap()
        }
    }

    pub fn origin_name(&self, t: &WhiteoutType) -> Option<&OsStr> {
        if let Some(name) = self.name().to_str() {
            if *t == WhiteoutType::OCIRemoval {
                // the whiteout filename prefixes the basename of the path to be deleted with ".wh.".
                return Some(OsStr::from_bytes(
                    name[OCISPEC_WHITEOUT_PREFIX.len()..].as_bytes(),
                ));
            } else if *t == WhiteoutType::OverlayFSRemoval {
                // the whiteout file has the same name as the file to be deleted.
                return Some(name.as_ref());
            }
        }

        None
    }

    pub fn path_vec(&self) -> Vec<OsString> {
        self.rootfs()
            .components()
            .map(|comp| match comp {
                Component::RootDir => OsString::from("/"),
                Component::Normal(name) => name.to_os_string(),
                _ => OsString::new(),
            })
            .collect::<Vec<_>>()
    }

    pub fn is_overlayfs_whiteout(&self, spec: &WhiteoutSpec) -> bool {
        if *spec != WhiteoutSpec::Overlayfs {
            return false;
        }

        (self.inode.i_mode & libc::S_IFMT == libc::S_IFCHR)
            && stat::major(self.rdev) == 0
            && stat::minor(self.rdev) == 0
    }

    fn is_overlayfs_opaque(&self, spec: &WhiteoutSpec) -> bool {
        if *spec != WhiteoutSpec::Overlayfs {
            return false;
        }

        // A directory is made opaque by setting the xattr
        // "trusted.overlay.opaque" to "y".
        if let Some(v) = self
            .xattrs
            .pairs
            .get(&OsString::from("trusted.overlay.opaque"))
        {
            if let Ok(v) = std::str::from_utf8(v.as_slice()) {
                return v == "y";
            }
        }

        false
    }

    pub fn whiteout_type(&self, spec: &WhiteoutSpec) -> Option<WhiteoutType> {
        if self.overlay == Overlay::Lower {
            return None;
        }

        match spec {
            WhiteoutSpec::Oci => {
                if let Some(name) = self.name().to_str() {
                    if name == OCISPEC_WHITEOUT_OPAQUE {
                        return Some(WhiteoutType::OCIOpaque);
                    } else if name.starts_with(OCISPEC_WHITEOUT_PREFIX) {
                        return Some(WhiteoutType::OCIRemoval);
                    }
                }
            }
            WhiteoutSpec::Overlayfs => {
                if self.is_overlayfs_whiteout(spec) {
                    return Some(WhiteoutType::OverlayFSRemoval);
                } else if self.is_overlayfs_opaque(spec) {
                    return Some(WhiteoutType::OverlayFSOpaque);
                }
            }
        }

        None
    }
}
