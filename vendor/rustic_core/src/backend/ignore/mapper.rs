#[cfg(not(windows))]
pub mod nix_mapper;

use std::{ffi::OsStr, path::Path};

use derive_setters::Setters;
use ignore::DirEntry;
use jiff::Timestamp;
use log::warn;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use super::{IgnoreErrorKind, IgnoreResult, OpenFile};
use crate::backend::{
    ReadSourceEntry,
    node::{
        ExtendedAttribute, Metadata, Node, NodeType,
        modification::{DevIdOption, TimeOption, XattrOption},
    },
};

#[cfg(not(windows))]
use std::os::unix::fs::{FileTypeExt, MetadataExt};

#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BlockdevOption {
    #[default]
    Special,
    File,
}

#[serde_as]
#[cfg_attr(feature = "clap", derive(clap::Parser))]
#[cfg_attr(feature = "merge", derive(conflate::Merge))]
#[derive(serde::Deserialize, serde::Serialize, Default, Clone, Copy, Debug, Setters)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
#[setters(into)]
#[non_exhaustive]
/// [`LocalSourceSaveOptions`] describes how entries from a local source will be saved in the repository.
pub struct LocalSourceSaveOptions {
    /// Set access time [default: mtime]
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub set_atime: Option<TimeOption>,

    /// Set changed time [default: yes]
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub set_ctime: Option<TimeOption>,

    /// Set device ID [default: hardlink]
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub set_devid: Option<DevIdOption>,

    /// How block devices should be stored [default: special]
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub set_blockdev: Option<BlockdevOption>,

    /// Set extended attributes [default: yes]
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub set_xattrs: Option<XattrOption>,

    /// Set the times stored for directories [default: yes]
    ///
    /// A directory's times are the times of the writes *into* it, not of
    /// anything it holds: creating, renaming or removing an entry updates
    /// them, and on Windows the filesystem reports the new value lazily, so a
    /// directory written to shortly before a walk can hand one value to that
    /// walk and another to the next one. A node that stores such a value
    /// therefore moves on its own, and a tree serializes its nodes, so the
    /// tree (and every tree above it, whose child reference changed) is
    /// written again with nothing changed. How a directory is compared for
    /// *content* does not read this: a directory is compared through the id of
    /// the tree it holds, so `no` drops nothing the comparison uses. Set this
    /// to `no` when a source's directory times are not meaningful to the
    /// backup and a tree must not move without one — file nodes are not
    /// affected by this option, and a file's mtime remains what change
    /// detection reads.
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub set_dir_times: Option<TimeOption>,
}

impl LocalSourceSaveOptions {
    /// Maps a [`DirEntry`] to a [`ReadSourceEntry`].
    ///
    /// # Arguments
    ///
    /// * `entry` - The [`DirEntry`] to map.
    /// * `options` - options for saving entries
    ///
    /// # Errors
    ///
    /// * If metadata could not be read.
    /// * If the xattr of the entry could not be read.
    pub fn map_entry(self, entry: DirEntry) -> IgnoreResult<ReadSourceEntry<OpenFile>> {
        let name = entry.file_name();
        let m = entry
            .metadata()
            .map_err(|err| IgnoreErrorKind::AcquiringMetadataFailed {
                name: name.to_string_lossy().to_string(),
                source: err,
            })?;

        let mtime = m.modified().ok().and_then(|t| Timestamp::try_from(t).ok());
        let atime = || m.accessed().ok().and_then(|t| Timestamp::try_from(t).ok());
        let atime = self
            .set_atime
            .unwrap_or(TimeOption::Mtime)
            .map_or_else(atime, mtime);
        let ctime = || Self::ctime(&m);
        let ctime = self
            .set_ctime
            .unwrap_or(TimeOption::Yes)
            .map_or_else(ctime, mtime);

        let (uid, user, gid, group) = Self::user_group(&m);
        let size = if m.is_dir() { 0 } else { m.len() };
        let device_id = self
            .set_devid
            .unwrap_or_default()
            .map_or_else(|| Self::device_id(&m), Self::hardlink(&m));
        let xattr = || {
            Self::xattrs(entry.path())
                .inspect_err(|err| warn!("ignoring error obtaining xargs: {err}"))
                .unwrap_or_default()
        };
        let extended_attributes = self.set_xattrs.unwrap_or_default().map_or_else(xattr);
        let (mode, inode, links) = Self::nix_infos(&m);
        // The directory half of the time policy, applied last: a directory
        // node's times are the times of the writes into it, and dropping them
        // takes nothing the content comparison reads (see `set_dir_times`).
        let dir_times = self.set_dir_times.unwrap_or(TimeOption::Yes);
        let time = |t: Option<Timestamp>| match (m.is_dir(), dir_times) {
            (false, _) | (true, TimeOption::Yes) => t,
            (true, TimeOption::Mtime) => mtime,
            (true, TimeOption::No) => None,
        };

        let meta = Metadata {
            mode,
            mtime: time(mtime),
            atime: time(atime),
            ctime: time(ctime),
            uid,
            gid,
            user,
            group,
            inode,
            device_id,
            size,
            links,
            extended_attributes,
        };

        let node = self.to_node(&entry, &m, meta)?;
        let path = entry.into_path();
        let open = Some(OpenFile(path.clone()));
        Ok(ReadSourceEntry { path, node, open })
    }

    fn to_node(
        self,
        entry: &DirEntry,
        m: &std::fs::Metadata,
        meta: Metadata,
    ) -> IgnoreResult<Node> {
        let name = entry.file_name();
        let node = if m.is_dir() {
            Node::new_node(name, NodeType::Dir, meta)
        } else if m.is_symlink() {
            let path = entry.path();
            let target = std::fs::read_link(path).map_err(|err| IgnoreErrorKind::ErrorLink {
                path: path.to_path_buf(),
                source: err,
            })?;
            let node_type = NodeType::from_link(&target);
            Node::new_node(name, node_type, meta)
        } else {
            self.to_node_other(name, m, meta)
        };
        Ok(node)
    }
}

#[cfg(not(windows))]
impl LocalSourceSaveOptions {
    fn ctime(m: &std::fs::Metadata) -> Option<Timestamp> {
        #[allow(clippy::cast_possible_truncation)]
        Timestamp::new(m.ctime(), m.ctime_nsec() as i32).ok()
    }

    fn device_id(m: &std::fs::Metadata) -> u64 {
        m.dev()
    }

    fn hardlink(m: &std::fs::Metadata) -> bool {
        m.nlink() > 1 && !m.is_dir()
    }

    fn user_group(
        m: &std::fs::Metadata,
    ) -> (Option<u32>, Option<String>, Option<u32>, Option<String>) {
        let uid = m.uid();
        let gid = m.gid();
        let user = nix_mapper::get_user_by_uid(uid);
        let group = nix_mapper::get_group_by_gid(gid);
        (Some(uid), user, Some(gid), group)
    }

    fn nix_infos(m: &std::fs::Metadata) -> (Option<u32>, u64, u64) {
        let mode = nix_mapper::map_mode_to_go(m.mode());
        let inode = m.ino();
        let links = if m.is_dir() { 0 } else { m.nlink() };
        (Some(mode), inode, links)
    }

    /// List [`ExtendedAttribute`] for a [`Node`] located at `path`
    ///
    /// # Argument
    ///
    /// * `path` to the [`Node`] for which to list attributes
    ///
    /// # Errors
    ///
    /// * If Xattr couldn't be listed or couldn't be read
    #[cfg(not(target_os = "openbsd"))]
    fn xattrs(path: &Path) -> IgnoreResult<Vec<ExtendedAttribute>> {
        xattr::list(path)
            .map_err(|err| IgnoreErrorKind::ErrorXattr {
                path: path.to_path_buf(),
                source: err,
            })?
            .map(|name| {
                Ok(ExtendedAttribute {
                    name: name.to_string_lossy().to_string(),
                    value: xattr::get(path, name).map_err(|err| IgnoreErrorKind::ErrorXattr {
                        path: path.to_path_buf(),
                        source: err,
                    })?,
                })
            })
            .collect::<IgnoreResult<Vec<ExtendedAttribute>>>()
    }

    #[cfg(target_os = "openbsd")]
    fn xattrs(_path: &Path) -> IgnoreResult<Vec<ExtendedAttribute>> {
        Ok(Vec::new())
    }

    fn to_node_other(self, name: &OsStr, m: &std::fs::Metadata, meta: Metadata) -> Node {
        let filetype = m.file_type();
        if filetype.is_block_device() {
            if matches!(self.set_blockdev.unwrap_or_default(), BlockdevOption::File) {
                Node::new_node(name, NodeType::File, meta)
            } else {
                let node_type = NodeType::Dev { device: m.rdev() };
                Node::new_node(name, node_type, meta)
            }
        } else if filetype.is_char_device() {
            let node_type = NodeType::Chardev { device: m.rdev() };
            Node::new_node(name, node_type, meta)
        } else if filetype.is_fifo() {
            Node::new_node(name, NodeType::Fifo, meta)
        } else if filetype.is_socket() {
            Node::new_node(name, NodeType::Socket, meta)
        } else {
            Node::new_node(name, NodeType::File, meta)
        }
    }
}

#[cfg(windows)]
impl LocalSourceSaveOptions {
    fn ctime(m: &std::fs::Metadata) -> Option<Timestamp> {
        m.created().ok().and_then(|t| Timestamp::try_from(t).ok())
    }
    fn device_id(_m: &std::fs::Metadata) -> u64 {
        0
    }
    fn hardlink(m: &std::fs::Metadata) -> bool {
        false
    }
    fn user_group(
        _m: &std::fs::Metadata,
    ) -> (Option<u32>, Option<String>, Option<u32>, Option<String>) {
        (None, None, None, None)
    }

    fn nix_infos(_m: &std::fs::Metadata) -> (Option<u32>, u64, u64) {
        (None, 0, 0)
    }

    fn xattrs(_path: &Path) -> IgnoreResult<Vec<ExtendedAttribute>> {
        Ok(Vec::new())
    }

    fn to_node_other(self, name: &OsStr, _m: &std::fs::Metadata, meta: Metadata) -> Node {
        Node::new_node(name, NodeType::File, meta)
    }
}
