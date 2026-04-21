use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use afs_db::NodeKind;
use afs_hydrator::Hydrator;
use afs_resolver::{ResolvedNode, Resolver};
use fuser::{
    BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, Generation, INodeNo,
    LockOwner, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyWrite, Request, TimeOrNow, WriteFlags,
};
use tracing::warn;

use crate::inode::{InodeKind, InodeTable};

const TTL: Duration = Duration::from_secs(1);
const BLOCK_SIZE: u32 = 512;
const GITFILE_PATH: &str = ".git";

/// The AFS FUSE filesystem.
pub struct AfsFilesystem {
    pub resolver: Resolver,
    inodes: Mutex<InodeTable>,
    gitdir: PathBuf,
    runtime: tokio::runtime::Handle,
    /// Optional BlobStore for S3-backed reads (including LFS).
    blob_store: Option<Arc<afs_store::BlobStore>>,
    /// Optional LFS server URL for batch API fallback.
    lfs_server_url: Option<String>,
    /// Hydrator for lazy blob fetching.
    hydrator: Option<Hydrator>,
}

impl AfsFilesystem {
    pub fn new(resolver: Resolver, gitdir: PathBuf, runtime: tokio::runtime::Handle) -> Self {
        Self {
            resolver,
            inodes: Mutex::new(InodeTable::new()),
            gitdir,
            runtime,
            blob_store: None,
            lfs_server_url: None,
            hydrator: None,
        }
    }

    /// Set the blob store for S3-backed reads and start the hydrator.
    pub fn set_blob_store(&mut self, store: Arc<afs_store::BlobStore>) {
        self.blob_store = Some(store.clone());

        // Start the hydrator backed by this BlobStore
        let fetch_fn: afs_hydrator::FetchFn = Arc::new(move |oid: String| {
            let store = store.clone();
            tokio::spawn(async move {
                let data = store.get_blob(&oid).await?;
                Ok(data)
            })
        });
        self.hydrator = Some(Hydrator::start(4, fetch_fn));
    }

    /// Set the LFS server URL for batch API fallback.
    pub fn set_lfs_server_url(&mut self, url: String) {
        self.lfs_server_url = Some(url);
    }

    pub fn resolve_path(&self, path: &str) -> Option<ResolvedNode> {
        self.runtime.block_on(self.resolver.resolve(path)).ok()?
    }

    pub fn list_dir(&self, parent: &str) -> Vec<ResolvedNode> {
        self.runtime
            .block_on(self.resolver.list_dir(parent))
            .unwrap_or_default()
    }

    fn make_attr(&self, ino: INodeNo, node: &ResolvedNode) -> FileAttr {
        let kind = match node.kind {
            NodeKind::Dir => FileType::Directory,
            NodeKind::Blob | NodeKind::Lfs => FileType::RegularFile,
            NodeKind::Symlink => FileType::Symlink,
        };
        let perm = if node.kind == NodeKind::Dir {
            0o755
        } else {
            (node.mode & 0o777) as u16
        };
        let size = node.size.unwrap_or(0) as u64;
        let now = SystemTime::now();

        FileAttr {
            ino,
            size,
            blocks: size.div_ceil(BLOCK_SIZE as u64),
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind,
            perm,
            nlink: if node.kind == NodeKind::Dir { 2 } else { 1 },
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: BLOCK_SIZE,
            flags: 0,
        }
    }

    fn gitfile_attr(&self, ino: INodeNo) -> FileAttr {
        let size = self.gitfile_content().len() as u64;
        let now = SystemTime::now();
        FileAttr {
            ino,
            size,
            blocks: 1,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind: FileType::RegularFile,
            perm: 0o444,
            nlink: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: BLOCK_SIZE,
            flags: 0,
        }
    }

    pub fn gitfile_content(&self) -> Vec<u8> {
        format!("gitdir: {}\n", self.gitdir.display()).into_bytes()
    }

    /// Read blob via gix. Later: hydrator → cache → S3.
    fn read_blob(&self, oid: &str) -> Option<Vec<u8>> {
        let repo = gix::open(&self.gitdir).ok()?;
        let oid = gix::ObjectId::from_hex(oid.as_bytes()).ok()?;
        let obj = repo.find_object(oid).ok()?;
        Some(obj.data.to_vec())
    }

    fn path_of(&self, ino: INodeNo) -> Option<String> {
        let inodes = self.inodes.lock().unwrap();
        inodes.get(ino.0).map(|r| r.path.clone())
    }

    fn alloc_inode(&self, path: &str, kind: InodeKind, mode: u32) -> u64 {
        let mut inodes = self.inodes.lock().unwrap();
        inodes.get_or_insert(path, kind, mode)
    }

    pub fn child_path(parent: &str, name: &str) -> String {
        if parent == "." {
            name.to_string()
        } else {
            format!("{}/{}", parent, name)
        }
    }

    /// Ensure a base file is promoted to overlay for writing. Returns backing path.
    fn ensure_overlay(&self, path: &str, node: &ResolvedNode) -> Result<(), Errno> {
        if node.from_overlay {
            return Ok(());
        }

        let overlay = self.resolver.overlay().ok_or(Errno::EROFS)?;

        // Need to read the base blob first for copy-on-write
        let blob_data = match &node.oid {
            Some(oid) => self.read_blob(oid).unwrap_or_default(),
            None => Vec::new(),
        };

        let base = afs_db::BaseNode {
            generation: self.resolver.generation(),
            path: path.to_string(),
            parent: String::new(),
            kind: node.kind,
            oid: node.oid.clone(),
            mode: node.mode,
            size: node.size,
        };

        self.runtime
            .block_on(overlay.ensure_copy_on_write(path, &base, &blob_data))
            .map_err(|_| Errno::EIO)?;

        Ok(())
    }
}

impl Filesystem for AfsFilesystem {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => return reply.error(Errno::ENOENT),
        };

        let parent_path = match self.path_of(parent) {
            Some(p) => p,
            None => return reply.error(Errno::ENOENT),
        };

        if parent == INodeNo::ROOT && name_str == ".git" {
            let ino = self.alloc_inode(GITFILE_PATH, InodeKind::File, 0o444);
            reply.entry(&TTL, &self.gitfile_attr(INodeNo(ino)), Generation(0));
            return;
        }

        let child_path = Self::child_path(&parent_path, name_str);

        match self.resolve_path(&child_path) {
            Some(node) => {
                let kind = match node.kind {
                    NodeKind::Dir => InodeKind::Dir,
                    NodeKind::Symlink => InodeKind::Symlink,
                    _ => InodeKind::File,
                };
                let ino = self.alloc_inode(&child_path, kind, node.mode as u32);
                reply.entry(&TTL, &self.make_attr(INodeNo(ino), &node), Generation(0));
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let path = match self.path_of(ino) {
            Some(p) => p,
            None => return reply.error(Errno::ENOENT),
        };

        if path == GITFILE_PATH {
            return reply.attr(&TTL, &self.gitfile_attr(ino));
        }

        match self.resolve_path(&path) {
            Some(node) => reply.attr(&TTL, &self.make_attr(ino, &node)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let path = match self.path_of(ino) {
            Some(p) => p,
            None => return reply.error(Errno::ENOENT),
        };

        // Handle truncate (size change)
        if let Some(new_size) = size {
            let node = match self.resolve_path(&path) {
                Some(n) => n,
                None => return reply.error(Errno::ENOENT),
            };

            if let Err(e) = self.ensure_overlay(&path, &node) {
                return reply.error(e);
            }

            // Truncate the backing file
            if let Some(overlay) = self.resolver.overlay()
                && let Ok(Some(ovl_node)) = self.runtime.block_on(overlay.get(&path))
                && let Some(ref backing) = ovl_node.backing
                && let Ok(f) = std::fs::OpenOptions::new().write(true).open(backing)
            {
                let _ = f.set_len(new_size);
            }
        }

        // Re-resolve and return updated attrs
        match self.resolve_path(&path) {
            Some(node) => reply.attr(&TTL, &self.make_attr(ino, &node)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let path = match self.path_of(ino) {
            Some(p) => p,
            None => return reply.error(Errno::ENOENT),
        };

        if path == GITFILE_PATH {
            let content = self.gitfile_content();
            let start = offset as usize;
            if start >= content.len() {
                return reply.data(&[]);
            }
            let end = (start + size as usize).min(content.len());
            return reply.data(&content[start..end]);
        }

        let node = match self.resolve_path(&path) {
            Some(n) => n,
            None => return reply.error(Errno::ENOENT),
        };

        // If from overlay, read from backing file
        if node.from_overlay
            && let Some(ref backing) = node.backing_path
            && let Some(overlay) = self.resolver.overlay()
        {
            match overlay.read_file(backing, offset, size) {
                Ok(data) => return reply.data(&data),
                Err(_) => return reply.error(Errno::EIO),
            }
        }

        let oid = match &node.oid {
            Some(o) => o.clone(),
            None => return reply.error(Errno::EIO),
        };

        // LFS objects: fetch via BlobStore LFS path
        if node.kind == NodeKind::Lfs {
            if let Some(ref store) = self.blob_store {
                let lfs_url = self.lfs_server_url.as_deref();
                match self.runtime.block_on(store.get_lfs_object(&oid, lfs_url)) {
                    Ok(data) => {
                        let start = offset as usize;
                        if start >= data.len() {
                            return reply.data(&[]);
                        }
                        let end = (start + size as usize).min(data.len());
                        return reply.data(&data[start..end]);
                    }
                    Err(e) => {
                        warn!(%oid, %path, error = %e, "LFS object fetch failed");
                        return reply.error(Errno::EIO);
                    }
                }
            } else {
                warn!(%oid, %path, "LFS object requested but no BlobStore configured");
                return reply.error(Errno::EIO);
            }
        }

        // Regular blobs: hydrator → cache → S3, fallback to gix
        if let Some(ref hydrator) = self.hydrator {
            match self.runtime.block_on(hydrator.ensure_hydrated(&oid, &path)) {
                Ok(data) => {
                    let start = offset as usize;
                    if start >= data.len() {
                        return reply.data(&[]);
                    }
                    let end = (start + size as usize).min(data.len());
                    return reply.data(&data[start..end]);
                }
                Err(e) => {
                    // Fall through to gix
                    warn!(%oid, %path, error = %e, "hydrator fetch failed, trying gix");
                }
            }
        } else if let Some(ref store) = self.blob_store {
            match self.runtime.block_on(store.get_blob(&oid)) {
                Ok(data) => {
                    let start = offset as usize;
                    if start >= data.len() {
                        return reply.data(&[]);
                    }
                    let end = (start + size as usize).min(data.len());
                    return reply.data(&data[start..end]);
                }
                Err(e) => {
                    warn!(%oid, %path, error = %e, "BlobStore fetch failed, trying gix");
                }
            }
        }

        match self.read_blob(&oid) {
            Some(data) => {
                let start = offset as usize;
                if start >= data.len() {
                    reply.data(&[]);
                } else {
                    let end = (start + size as usize).min(data.len());
                    reply.data(&data[start..end]);
                }
            }
            None => {
                warn!(%oid, %path, "failed to read blob");
                reply.error(Errno::EIO);
            }
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let path = match self.path_of(ino) {
            Some(p) => p,
            None => return reply.error(Errno::ENOENT),
        };

        let node = match self.resolve_path(&path) {
            Some(n) => n,
            None => return reply.error(Errno::ENOENT),
        };

        // Ensure file is in overlay (copy-on-write)
        if let Err(e) = self.ensure_overlay(&path, &node) {
            return reply.error(e);
        }

        let overlay = match self.resolver.overlay() {
            Some(o) => o,
            None => return reply.error(Errno::EROFS),
        };

        match self.runtime.block_on(overlay.write_file(&path, offset, data)) {
            Ok(n) => reply.written(n as u32),
            Err(e) => {
                warn!(%path, error = %e, "write failed");
                reply.error(Errno::EIO);
            }
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let parent_path = match self.path_of(parent) {
            Some(p) => p,
            None => return reply.error(Errno::ENOENT),
        };

        let name_str = match name.to_str() {
            Some(n) => n,
            None => return reply.error(Errno::EINVAL),
        };

        let path = Self::child_path(&parent_path, name_str);

        let overlay = match self.resolver.overlay() {
            Some(o) => o,
            None => return reply.error(Errno::EROFS),
        };

        match self.runtime.block_on(overlay.create_file(&path, mode as i64)) {
            Ok(_node) => {
                let ino = self.alloc_inode(&path, InodeKind::File, mode);
                if let Some(resolved) = self.resolve_path(&path) {
                    let attr = self.make_attr(INodeNo(ino), &resolved);
                    reply.created(&TTL, &attr, Generation(0), FileHandle(0), fuser::FopenFlags::empty());
                } else {
                    reply.error(Errno::EIO);
                }
            }
            Err(e) => {
                warn!(%path, error = %e, "create failed");
                reply.error(Errno::EIO);
            }
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let parent_path = match self.path_of(parent) {
            Some(p) => p,
            None => return reply.error(Errno::ENOENT),
        };

        let name_str = match name.to_str() {
            Some(n) => n,
            None => return reply.error(Errno::EINVAL),
        };

        let path = Self::child_path(&parent_path, name_str);

        let overlay = match self.resolver.overlay() {
            Some(o) => o,
            None => return reply.error(Errno::EROFS),
        };

        match self.runtime.block_on(overlay.mkdir(&path, mode as i64)) {
            Ok(()) => {
                let ino = self.alloc_inode(&path, InodeKind::Dir, mode);
                if let Some(resolved) = self.resolve_path(&path) {
                    reply.entry(&TTL, &self.make_attr(INodeNo(ino), &resolved), Generation(0));
                } else {
                    reply.error(Errno::EIO);
                }
            }
            Err(e) => {
                warn!(%path, error = %e, "mkdir failed");
                reply.error(Errno::EIO);
            }
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let parent_path = match self.path_of(parent) {
            Some(p) => p,
            None => return reply.error(Errno::ENOENT),
        };

        let name_str = match name.to_str() {
            Some(n) => n,
            None => return reply.error(Errno::EINVAL),
        };

        let path = Self::child_path(&parent_path, name_str);

        let overlay = match self.resolver.overlay() {
            Some(o) => o,
            None => return reply.error(Errno::EROFS),
        };

        match self.runtime.block_on(overlay.remove(&path)) {
            Ok(()) => reply.ok(),
            Err(e) => {
                warn!(%path, error = %e, "unlink failed");
                reply.error(Errno::EIO);
            }
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let parent_path = match self.path_of(parent) {
            Some(p) => p,
            None => return reply.error(Errno::ENOENT),
        };

        let name_str = match name.to_str() {
            Some(n) => n,
            None => return reply.error(Errno::EINVAL),
        };

        let path = Self::child_path(&parent_path, name_str);

        // Check directory is empty
        let children = self.list_dir(&path);
        if !children.is_empty() {
            return reply.error(Errno::ENOTEMPTY);
        }

        let overlay = match self.resolver.overlay() {
            Some(o) => o,
            None => return reply.error(Errno::EROFS),
        };

        match self.runtime.block_on(overlay.remove(&path)) {
            Ok(()) => reply.ok(),
            Err(e) => {
                warn!(%path, error = %e, "rmdir failed");
                reply.error(Errno::EIO);
            }
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let parent_path = match self.path_of(parent) {
            Some(p) => p,
            None => return reply.error(Errno::ENOENT),
        };
        let newparent_path = match self.path_of(newparent) {
            Some(p) => p,
            None => return reply.error(Errno::ENOENT),
        };

        let old_name = match name.to_str() {
            Some(n) => n,
            None => return reply.error(Errno::EINVAL),
        };
        let new_name = match newname.to_str() {
            Some(n) => n,
            None => return reply.error(Errno::EINVAL),
        };

        let old_path = Self::child_path(&parent_path, old_name);
        let new_path = Self::child_path(&newparent_path, new_name);

        // Ensure old file is in overlay first
        if let Some(node) = self.resolve_path(&old_path)
            && let Err(e) = self.ensure_overlay(&old_path, &node)
        {
            return reply.error(e);
        }

        let overlay = match self.resolver.overlay() {
            Some(o) => o,
            None => return reply.error(Errno::EROFS),
        };

        match self.runtime.block_on(overlay.rename(&old_path, &new_path)) {
            Ok(()) => reply.ok(),
            Err(e) => {
                warn!(%old_path, %new_path, error = %e, "rename failed");
                reply.error(Errno::EIO);
            }
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let path = match self.path_of(ino) {
            Some(p) => p,
            None => return reply.error(Errno::ENOENT),
        };

        let children = self.list_dir(&path);
        let mut entries: Vec<(u64, FileType, String)> = Vec::new();

        entries.push((ino.0, FileType::Directory, ".".to_string()));
        entries.push((ino.0, FileType::Directory, "..".to_string()));

        if ino == INodeNo::ROOT {
            let git_ino = self.alloc_inode(GITFILE_PATH, InodeKind::File, 0o444);
            entries.push((git_ino, FileType::RegularFile, ".git".to_string()));
        }

        for child in &children {
            let ft = match child.kind {
                NodeKind::Dir => FileType::Directory,
                NodeKind::Symlink => FileType::Symlink,
                _ => FileType::RegularFile,
            };
            let kind = match child.kind {
                NodeKind::Dir => InodeKind::Dir,
                NodeKind::Symlink => InodeKind::Symlink,
                _ => InodeKind::File,
            };
            let child_ino = self.alloc_inode(&child.path, kind, child.mode as u32);
            entries.push((child_ino, ft, child.name().to_string()));
        }

        for (i, (ino_val, ft, name)) in entries.iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(*ino_val), (i + 1) as u64, *ft, name) {
                break;
            }
        }
        reply.ok();

        // Prefetch sibling blobs in the background
        if let Some(ref hydrator) = self.hydrator {
            for child in &children {
                if let Some(ref oid) = child.oid
                    && (child.kind == NodeKind::Blob || child.kind == NodeKind::Lfs)
                {
                    let priority = afs_hydrator::classify_priority(&child.path);
                    let task = afs_hydrator::HydrationTask {
                        oid: oid.clone(),
                        path: child.path.clone(),
                        priority,
                        reason: "sibling prefetch",
                        enqueued_at: std::time::Instant::now(),
                    };
                    self.runtime.block_on(hydrator.enqueue(task));
                }
            }
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let path = match self.path_of(ino) {
            Some(p) => p,
            None => return reply.error(Errno::ENOENT),
        };

        let node = match self.resolve_path(&path) {
            Some(n) if n.is_symlink() => n,
            _ => return reply.error(Errno::EINVAL),
        };

        let oid = match &node.oid {
            Some(o) => o.clone(),
            None => return reply.error(Errno::EIO),
        };

        match self.read_blob(&oid) {
            Some(data) => reply.data(&data),
            None => reply.error(Errno::EIO),
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        let mut inodes = self.inodes.lock().unwrap();
        inodes.forget(ino.0, nlookup);
    }
}
