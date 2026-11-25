//! File operations relative to an opened directory. The selected album (or an
//! explicitly specified output directory) is the only ambient trust decision.
//! Inside it, don't follow symbolic links, junctions or other reparse points.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::fs::{Dir, OpenOptions};

const STATE_DIR: &str = ".firstcut";

pub fn absolute_output(path: Option<PathBuf>) -> io::Result<Option<PathBuf>> {
    path.map(std::path::absolute).transpose()
}

fn denied(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

fn one_name(path: &Path) -> io::Result<&OsStr> {
    match path.components().next() {
        Some(Component::Normal(name)) if path.as_os_str() == name => {
            #[cfg(windows)]
            {
                // Refuse alternate data streams and Win32's trimmed aliases.
                let text = name.to_string_lossy();
                if text.contains(':') || text.ends_with(['.', ' ']) {
                    return Err(denied("文件名不能包含 Windows 数据流或别名"));
                }
            }
            Ok(name)
        }
        _ => Err(denied("路径必须是目录内的单个普通文件名")),
    }
}

pub struct Folder(Dir);

impl Folder {
    pub fn open(path: &Path) -> io::Result<Self> {
        let absolute = std::path::absolute(path)?;
        // Start from the filesystem/volume root rather than trusting a
        // multi-component ambient path to remain resolved between checks.
        let mut base = PathBuf::new();
        let mut components = absolute.components().peekable();
        while let Some(Component::Prefix(_) | Component::RootDir) = components.peek() {
            base.push(components.next().unwrap().as_os_str());
        }
        let dir = Dir::open_ambient_dir(&base, cap_std::ambient_authority())?;
        check_directory(dir.dir_metadata()?)?;
        Self(dir).descend(components.collect::<PathBuf>().as_path(), false)
    }

    // Explicit outputs are a separate trust decision; paths under the album
    // always use open(), including every component of the album-relative path.
    fn open_explicit(path: &Path) -> io::Result<Self> {
        let dir = Dir::open_ambient_dir(path, cap_std::ambient_authority())?;
        check_directory(dir.dir_metadata()?)?;
        Ok(Self(dir))
    }

    /// 打开已存在的显式目录，并拒绝最后一段路径本身为符号链接或重解析点。
    fn open_explicit_nofollow(path: &Path) -> io::Result<Self> {
        if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
            if !parent.as_os_str().is_empty() {
                return Self::open_explicit(parent)?.child(name, false);
            }
        }
        Self::open_explicit(path)
    }

    /// 打开或安全创建显式目标目录（用于导出到相册外的已授权目录）：
    /// 从已存在的最近祖先起逐级无链接创建并校验每一层不是符号链接或重解析点。
    pub fn open_or_create_explicit(path: &Path, create: bool) -> io::Result<Self> {
        let absolute = std::path::absolute(path)?;
        if !create {
            return Self::open_explicit_nofollow(&absolute);
        }
        // 找到已存在的最近祖先目录（含悬空符号链接），避免 create_dir_all 盲目穿越符号链接
        let mut existing = absolute.as_path();
        let mut missing: Vec<OsString> = Vec::new();
        loop {
            match existing.symlink_metadata() {
                Ok(_) => break,
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    let name = existing
                        .file_name()
                        .ok_or_else(|| denied("目标路径缺少有效祖先目录"))?;
                    missing.push(name.to_os_string());
                    existing = existing
                        .parent()
                        .ok_or_else(|| denied("目标路径缺少有效祖先目录"))?;
                }
                Err(err) => return Err(err),
            }
        }
        let mut dir = Self::open_explicit_nofollow(existing)?;
        for name in missing.into_iter().rev() {
            dir = dir.child(&name, true)?;
        }
        Ok(dir)
    }

    /// 打开相册根目录内的相对子目录（逐级无链接校验；`create = true` 时按需创建）。
    pub fn open_album_subdir(root: &Path, relative: &Path, create: bool) -> io::Result<Self> {
        let dir = Self::open(root)?;
        dir.descend(relative, create)
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self(self.0.try_clone()?))
    }

    pub fn descend_cloned(&self, relative: &Path, create: bool) -> io::Result<Self> {
        self.try_clone()?.descend(relative, create)
    }

    pub fn child(&self, name: impl AsRef<Path>, create: bool) -> io::Result<Self> {
        let name = name.as_ref();
        one_name(name)?;
        let dir = match self.0.open_dir_nofollow(name) {
            Ok(dir) => dir,
            Err(err) if err.kind() == io::ErrorKind::NotFound && create => {
                let result = self.0.create_dir(name);
                if let Err(err) = result {
                    if err.kind() != io::ErrorKind::AlreadyExists {
                        return Err(err);
                    }
                }
                self.0.open_dir_nofollow(name)?
            }
            Err(err) => return Err(err),
        };
        check_directory(dir.dir_metadata()?)?;
        Ok(Self(dir))
    }

    pub fn state(&self, create: bool) -> io::Result<Self> {
        self.child(STATE_DIR, create)
    }

    fn descend(self, relative: &Path, create: bool) -> io::Result<Self> {
        let mut dir = self;
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(denied("拒绝相册目录之外的路径"));
            };
            dir = dir.child(name, create)?;
        }
        Ok(dir)
    }

    pub fn file(&self, name: impl AsRef<Path>) -> io::Result<FileRef> {
        let name = one_name(name.as_ref())?.to_os_string();
        Ok(FileRef {
            parent: Self(self.0.try_clone()?),
            name,
        })
    }

    /// Unlinking a name never opens the target.
    pub fn unlink(&self, name: impl AsRef<Path>) -> io::Result<()> {
        self.0.remove_file(one_name(name.as_ref())?)
    }

    /// 检查目录下是否包含未被 `marker_name` 标记的原始照片文件（无链接逐级遍历）。
    pub fn contains_unmarked_photos(
        &self,
        marker_name: &str,
        is_photo_name: impl Fn(&str) -> bool,
    ) -> io::Result<bool> {
        if self.file(marker_name)?.open_read()?.is_some() {
            return Ok(false);
        }
        const MAX_DEPTH: usize = 64;
        let mut stack = vec![(self.try_clone()?, 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            for entry in dir.0.entries()? {
                let entry = entry?;
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.eq_ignore_ascii_case(STATE_DIR) {
                    continue;
                }
                let meta = match dir.0.symlink_metadata(&name) {
                    Ok(m) => m,
                    Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                    Err(err) => return Err(err),
                };
                if is_reparse(&meta) {
                    continue;
                }
                if meta.is_file() {
                    if is_photo_name(&name_str) {
                        return Ok(true);
                    }
                } else if meta.is_dir() && depth < MAX_DEPTH {
                    let child = match dir.child(&name, false) {
                        Ok(c) => c,
                        Err(err)
                            if matches!(
                                err.kind(),
                                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                            ) =>
                        {
                            continue;
                        }
                        Err(err) => return Err(err),
                    };
                    if child
                        .file(marker_name)
                        .and_then(|f| f.open_read())
                        .ok()
                        .flatten()
                        .is_some()
                    {
                        continue;
                    }
                    stack.push((child, depth + 1));
                }
            }
        }
        Ok(false)
    }

    /// 非递归列出当前目录下所有普通文件的文件名（跳过符号链接与重解析点）。
    pub fn list_regular_filenames(&self) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in self.0.entries()? {
            let entry = entry?;
            let name = entry.file_name();
            let meta = match self.0.symlink_metadata(&name) {
                Ok(m) => m,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err),
            };
            if meta.is_file() && !is_reparse(&meta) {
                names.push(name.to_string_lossy().into_owned());
            }
        }
        Ok(names)
    }
}

fn check_directory(metadata: cap_std::fs::Metadata) -> io::Result<()> {
    if !metadata.is_dir() || is_reparse(&metadata) {
        return Err(denied(
            "拒绝使用链接、重解析点或非目录作为 firstcut 存储目录",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn is_reparse(metadata: &cap_std::fs::Metadata) -> bool {
    use cap_fs_ext::OsMetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse(metadata: &cap_std::fs::Metadata) -> bool {
    metadata.is_symlink()
}

/// 导出写入结果：成功写入或因目标在规划后发生变化而保护既有文件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportWriteOutcome {
    Written {
        bytes: u64,
        mtime_nanos: Option<i64>,
    },
    Conflict,
}

pub struct FileRef {
    parent: Folder,
    name: OsString,
}

impl FileRef {
    /// An output outside the album is honored only when explicitly supplied by
    /// the caller (e.g. --output or --cache). Within the album, every component
    /// below the selected root is opened without following links.
    pub fn at(root: &Path, path: &Path, create_parent: bool) -> io::Result<Self> {
        if let Some(rel) = strip_album_prefix(root, path) {
            let (parent, name) = split(&rel)?;
            return Folder::open(root)?
                .descend(parent, create_parent)?
                .file(name);
        }
        Self::explicit(path, create_parent)
    }

    /// For data that must stay inside the selected root, such as images and XMP.
    pub fn inside(root: &Path, path: &Path) -> io::Result<Self> {
        let rel = strip_album_prefix(root, path)
            .ok_or_else(|| denied("路径不在相册内"))?;
        let (parent, name) = split(&rel)?;
        Folder::open(root)?.descend(parent, false)?.file(name)
    }

    pub fn explicit(path: &Path, create_parent: bool) -> io::Result<Self> {
        let (parent, name) = split(path)?;
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        if create_parent {
            std::fs::create_dir_all(parent)?;
        }
        Folder::open_explicit(parent)?.file(name)
    }

    pub fn sibling(&self, suffix: &str) -> io::Result<Self> {
        let mut name = self.name.clone();
        name.push(suffix);
        self.parent.file(name)
    }

    fn check_entry(&self) -> io::Result<bool> {
        match self.parent.0.symlink_metadata(&self.name) {
            Ok(meta) if meta.is_file() && !is_reparse(&meta) => Ok(true),
            Ok(_) => Err(denied("拒绝访问链接、重解析点或非普通文件")),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }

    fn open(&self, options: &OpenOptions) -> io::Result<File> {
        let mut options = options.clone();
        // Nonblocking also prevents a replaced Unix FIFO from hanging the
        // open between the initial metadata check and the handle check.
        options.follow(FollowSymlinks::No).nonblock(true);
        let file = self.parent.0.open_with(&self.name, &options)?.into_std();
        check_handle(&file)?;
        Ok(file)
    }

    pub fn open_read(&self) -> io::Result<Option<File>> {
        if !self.check_entry()? {
            return Ok(None);
        }
        self.open(OpenOptions::new().read(true)).map(Some)
    }

    pub fn read(&self, limit: u64) -> io::Result<Option<Vec<u8>>> {
        let Some(file) = self.open_read()? else {
            return Ok(None);
        };
        let mut data = Vec::new();
        file.take(limit + 1).read_to_end(&mut data)?;
        if data.len() as u64 > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "文件超过大小上限",
            ));
        }
        Ok(Some(data))
    }

    pub fn open_update(&self) -> io::Result<File> {
        self.check_entry()?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        self.open(&options)
    }

    pub fn check_opened(&self, file: &File) -> io::Result<()> {
        check_handle(file)
    }

    /// 安全读取目标文件的大小与修改时间指纹（不跟随符号链接/重解析点）。
    pub fn fingerprint(&self) -> io::Result<Option<(u64, i64)>> {
        let Some(file) = self.open_read()? else {
            return Ok(None);
        };
        let meta = file.metadata()?;
        Ok(crate::cache::metadata_fingerprint(&meta))
    }

    pub fn write(&self, data: &[u8]) -> io::Result<()> {
        self.replace(|file| file.write_all(data))
    }

    /// 导出场景专用：按流式分块将已安全打开的源文件复制到临时文件，保留修改时间；
    /// 在原子重命名覆盖目标前执行 `check_before_rename`，若返回 `false` 则删除临时文件并返回 `Conflict`。
    pub fn copy_from_reader_checked(
        &self,
        src: &mut File,
        preserve_mtime: Option<std::time::SystemTime>,
        check_before_rename: impl FnOnce(&Self) -> io::Result<bool>,
    ) -> io::Result<ExportWriteOutcome> {
        self.open_read()?; // 拒绝已存在的硬链接/重解析点
        let mut entropy = [0u8; 16];
        getrandom::fill(&mut entropy).map_err(|err| io::Error::other(err.to_string()))?;
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
        let temp = format!(".firstcut-{}.tmp", URL_SAFE_NO_PAD.encode(entropy));
        let temp_ref = self.parent.file(&temp)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(0o644);
        }
        let mut file = temp_ref.open(&options)?;
        let result = (|| {
            let mut buf = vec![0u8; 256 * 1024];
            let mut total = 0u64;
            loop {
                let n = src.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                file.write_all(&buf[..n])?;
                total += n as u64;
            }
            if let Some(mtime) = preserve_mtime {
                // 在 sync 与 rename 之前写回源修改时间；若目标文件系统不支持则降级忽略
                let _ = file.set_modified(mtime);
            }
            file.sync_all()?;
            check_handle(&file)?;
            let actual_mtime = file
                .metadata()
                .ok()
                .and_then(|m| crate::cache::metadata_fingerprint(&m))
                .map(|(_, mt)| mt);
            drop(file);
            if !check_before_rename(self)? {
                let _ = self.parent.unlink(&temp);
                return Ok(ExportWriteOutcome::Conflict);
            }
            self.parent.0.rename(&temp, &self.parent.0, &self.name)?;
            Ok(ExportWriteOutcome::Written {
                bytes: total,
                mtime_nanos: actual_mtime,
            })
        })();
        if result.is_err() {
            let _ = self.parent.unlink(&temp);
        }
        result
    }

    /// 导出场景专用：写出完整字节内容（如 XMP 侧车）；
    /// 在原子重命名覆盖目标前执行 `check_before_rename`，若返回 `false` 则删除临时文件并返回 `Conflict`。
    pub fn write_exported_bytes_checked(
        &self,
        data: &[u8],
        check_before_rename: impl FnOnce(&Self) -> io::Result<bool>,
    ) -> io::Result<ExportWriteOutcome> {
        self.open_read()?;
        let mut entropy = [0u8; 16];
        getrandom::fill(&mut entropy).map_err(|err| io::Error::other(err.to_string()))?;
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
        let temp = format!(".firstcut-{}.tmp", URL_SAFE_NO_PAD.encode(entropy));
        let temp_ref = self.parent.file(&temp)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(0o644);
        }
        let mut file = temp_ref.open(&options)?;
        let result = (|| {
            file.write_all(data)?;
            file.sync_all()?;
            check_handle(&file)?;
            let actual_mtime = file
                .metadata()
                .ok()
                .and_then(|m| crate::cache::metadata_fingerprint(&m))
                .map(|(_, mt)| mt);
            drop(file);
            if !check_before_rename(self)? {
                let _ = self.parent.unlink(&temp);
                return Ok(ExportWriteOutcome::Conflict);
            }
            self.parent.0.rename(&temp, &self.parent.0, &self.name)?;
            Ok(ExportWriteOutcome::Written {
                bytes: data.len() as u64,
                mtime_nanos: actual_mtime,
            })
        })();
        if result.is_err() {
            let _ = self.parent.unlink(&temp);
        }
        result
    }

    /// Never truncate a named file. Write a new file and then replace the
    /// directory entry. Readers see either the old or the complete new file.
    pub fn replace(&self, write: impl FnOnce(&mut File) -> io::Result<()>) -> io::Result<()> {
        self.open_read()?; // Reject a pre-existing hard link as well.
        let mut entropy = [0u8; 16];
        getrandom::fill(&mut entropy).map_err(|err| io::Error::other(err.to_string()))?;
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
        let temp = format!(".firstcut-{}.tmp", URL_SAFE_NO_PAD.encode(entropy));
        let temp_ref = self.parent.file(&temp)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = temp_ref.open(&options)?;
        let result = (|| {
            write(&mut file)?;
            file.sync_all()?;
            check_handle(&file)?;
            // Closing before rename is required on some Windows filesystems.
            drop(file);
            self.parent.0.rename(&temp, &self.parent.0, &self.name)
        })();
        if result.is_err() {
            let _ = self.parent.unlink(&temp);
        }
        result
    }
}

fn canonicalize_existing_prefix(abs: &Path) -> PathBuf {
    let mut existing = abs;
    let mut tail: Vec<OsString> = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            return abs.to_path_buf();
        };
        let Some(parent) = existing.parent() else {
            return abs.to_path_buf();
        };
        tail.push(name.to_os_string());
        existing = parent;
    }
    let Ok(canon) = std::fs::canonicalize(existing) else {
        return abs.to_path_buf();
    };
    #[cfg(windows)]
    let mut base = {
        let text = canon.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            PathBuf::from(format!(r"\\{rest}"))
        } else if let Some(rest) = text.strip_prefix(r"\\?\") {
            PathBuf::from(rest)
        } else {
            canon
        }
    };
    #[cfg(not(windows))]
    let mut base = canon;
    for part in tail.into_iter().rev() {
        base.push(part);
    }
    base
}

/// 若 `path` 位于 `root` 之下（经 `std::path::absolute` 与已存在前缀规范化，消除 Windows 8.3 短路径差异，Windows 大小写不敏感），
/// 返回相对 `root` 的路径。
pub fn relative_under_album(root: &Path, path: &Path) -> io::Result<Option<PathBuf>> {
    let abs_root = canonicalize_existing_prefix(&std::path::absolute(root)?);
    let abs_path = canonicalize_existing_prefix(&std::path::absolute(path)?);
    Ok(strip_album_prefix(&abs_root, &abs_path))
}

/// 查询目标路径所在卷的可用剩余字节数（沿祖先找到最近已存在目录；不支持平台返回 None）。
pub fn available_disk_space(path: &Path) -> Option<u64> {
    let abs = std::path::absolute(path).ok()?;
    let existing = abs.ancestors().find(|p| p.exists())?;
    available_disk_space_inner(existing)
}

#[cfg(windows)]
fn available_disk_space_inner(existing: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide: Vec<u16> = existing.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free_to_caller = 0u64;
    let mut total_bytes = 0u64;
    let mut total_free = 0u64;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_to_caller,
            &mut total_bytes,
            &mut total_free,
        )
    };
    (ok != 0).then_some(free_to_caller)
}

#[cfg(not(windows))]
fn available_disk_space_inner(_existing: &Path) -> Option<u64> {
    None
}

fn strip_album_prefix(root: &Path, path: &Path) -> Option<PathBuf> {
    #[cfg(not(windows))]
    {
        path.strip_prefix(root).ok().map(Path::to_path_buf)
    }
    #[cfg(windows)]
    {
        let mut root_iter = root.components();
        let mut path_iter = path.components();
        loop {
            match root_iter.next() {
                None => return Some(path_iter.collect()),
                Some(r) => {
                    let p = path_iter.next()?;
                    let same = match (r, p) {
                        (Component::Prefix(a), Component::Prefix(b)) => a
                            .as_os_str()
                            .to_string_lossy()
                            .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy()),
                        (Component::RootDir, Component::RootDir)
                        | (Component::CurDir, Component::CurDir)
                        | (Component::ParentDir, Component::ParentDir) => true,
                        (Component::Normal(a), Component::Normal(b)) => a
                            .to_string_lossy()
                            .eq_ignore_ascii_case(&b.to_string_lossy()),
                        _ => false,
                    };
                    if !same {
                        return None;
                    }
                }
            }
        }
    }
}

fn split(path: &Path) -> io::Result<(&Path, &OsStr)> {
    let name = path.file_name().ok_or_else(|| denied("缺少文件名"))?;
    let parent = path.parent().ok_or_else(|| denied("缺少文件所在目录"))?;
    Ok((parent, name))
}

fn check_handle(file: &File) -> io::Result<()> {
    let meta = file.metadata()?;
    if !meta.is_file() || link_count(file, &meta)? != 1 {
        return Err(denied("拒绝访问硬链接或非普通文件"));
    }
    Ok(())
}

#[cfg(unix)]
fn link_count(_: &File, metadata: &std::fs::Metadata) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(metadata.nlink())
}

#[cfg(windows)]
fn link_count(file: &File, metadata: &std::fs::Metadata) -> io::Result<u64> {
    use std::os::windows::{fs::MetadataExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT,
    };
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(denied("拒绝访问重解析点"));
    }
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // The live File owns the handle, and info is a valid out parameter.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.nNumberOfLinks.into())
}

/// SQLite sees a byte stream, never a filename in the possibly untrusted album.
/// In particular it can't discover or write another file while recovering a
/// journal. Firstcut has always used the default rollback journal, so a
/// normally closed legacy database can be opened without an on-disk migration.
pub(crate) struct Database {
    pub conn: rusqlite::Connection,
    destination: FileRef,
    _lock: Option<File>,
}

impl Database {
    const MAX_BYTES: u64 = 128 * 1024 * 1024;

    pub fn open(destination: FileRef, write: bool) -> anyhow::Result<Option<Self>> {
        let lock = if write {
            let lock = destination.sibling(".firstcut-lock")?.open_update()?;
            anyhow::ensure!(
                lock.try_lock().is_ok(),
                "评分库正被另一进程写入，请稍后重试"
            );
            Some(lock)
        } else {
            None
        };
        Self::check_side_files(&destination)?;
        let mut conn = rusqlite::Connection::open_in_memory()?;
        conn.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
        conn.set_db_config(
            rusqlite::config::DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA,
            false,
        )?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        if let Some(mut file) = destination.open_read()? {
            use std::io::{Seek, SeekFrom};
            let size = file.metadata()?.len();
            anyhow::ensure!(
                (100..=Self::MAX_BYTES).contains(&size),
                "评分数据库大小异常；请保留原文件并检查数据库"
            );
            let mut header = [0; 20];
            file.read_exact(&mut header)?;
            anyhow::ensure!(
                &header[..16] == b"SQLite format 3\0" && header[18..20] == [1, 1],
                "仅加载正常关闭的 SQLite 评分库；有活动日志的库请先在可信目录中恢复"
            );
            file.seek(SeekFrom::Start(0))?;
            conn.deserialize_read_exact(rusqlite::MAIN_DB, file, size as usize, !write)?;
        } else if !write {
            return Ok(None);
        }
        if write {
            // Bound the in-memory database as well as the exported file.
            let page_size =
                conn.pragma_query_value(None, "page_size", |row| row.get::<_, i64>(0))? as u64;
            anyhow::ensure!((512..=65_536).contains(&page_size), "评分库页大小无效");
            conn.pragma_update(None, "max_page_count", (Self::MAX_BYTES / page_size) as i64)?;
        }
        Ok(Some(Self {
            conn,
            destination,
            _lock: lock,
        }))
    }

    fn check_side_files(destination: &FileRef) -> anyhow::Result<()> {
        for suffix in ["-journal", "-wal", "-shm"] {
            if let Some(file) = destination.sibling(suffix)?.open_read()? {
                anyhow::ensure!(
                    file.metadata()?.len() == 0,
                    "评分库存在 {suffix} 文件；请先在可信目录中恢复并正常关闭原库"
                );
            }
        }
        Ok(())
    }

    pub fn save(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(self._lock.is_some(), "不能保存只读评分库");
        Self::check_side_files(&self.destination)?;
        let data = self.conn.serialize(rusqlite::MAIN_DB)?;
        anyhow::ensure!(
            data.len() as u64 <= Self::MAX_BYTES,
            "评分数据库超过大小上限"
        );
        self.destination.write(&data)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regular_state_file_is_replaced_and_linked_state_is_not_opened() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("album");
        std::fs::create_dir(&root).unwrap();
        let state = Folder::open(&root).unwrap().state(true).unwrap();
        let file = state.file("ui-preferences.json").unwrap();
        file.write(b"old").unwrap();
        file.write(b"new").unwrap();
        assert_eq!(file.read(10).unwrap().unwrap(), b"new");

        let independent = temp.path().join("independent.txt");
        std::fs::write(&independent, b"keep original").unwrap();
        std::fs::hard_link(&independent, root.join(".firstcut/linked.json")).unwrap();
        let linked = state.file("linked.json").unwrap();
        assert!(linked.open_read().is_err());
        assert!(linked.open_update().is_err());
        assert!(linked.write(b"new").is_err());
        assert_eq!(std::fs::read(independent).unwrap(), b"keep original");
    }

    #[cfg(unix)]
    #[test]
    fn state_children_do_not_follow_directory_links() {
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("other");
        let root = temp.path().join("album");
        std::fs::create_dir(&outside).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink(&outside, root.join(".firstcut")).unwrap();
        assert!(Folder::open(&root).unwrap().state(true).is_err());
        assert!(std::fs::read_dir(outside).unwrap().next().is_none());
    }

    #[test]
    fn old_closed_sqlite_loads_without_creating_state_and_with_patched_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("legacy.sqlite");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE manual_rating(photo_path TEXT PRIMARY KEY, stars INTEGER);
                                INSERT INTO manual_rating VALUES ('sample.jpg',5);",
            )
            .unwrap();
        }
        let old = std::fs::read(&path).unwrap();
        let file = FileRef::explicit(&path, false).unwrap();
        let db = Database::open(file, false).unwrap().unwrap();
        assert!(rusqlite::version_number() >= 3_054_000);
        let stars: u8 = db
            .conn
            .query_row("SELECT stars FROM manual_rating", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stars, 5);
        assert_eq!(std::fs::read(&path).unwrap(), old);
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);

        // A database with pending recovery is preserved for the owner.
        let wal = temp.path().join("legacy.sqlite-wal");
        std::fs::write(&wal, b"pending recovery").unwrap();
        let file = FileRef::explicit(&path, false).unwrap();
        assert!(Database::open(file, false).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), old);
        assert_eq!(std::fs::read(&wal).unwrap(), b"pending recovery");
    }

    #[cfg(windows)]
    #[test]
    fn windows_strip_album_prefix_is_case_insensitive() {
        let root = Path::new(r"F:\Photos\Event");
        let lower = Path::new(r"f:\photos\event\.firstcut\report.csv");
        let rel = strip_album_prefix(root, lower).expect("应识别不同大小写的相册内路径");
        assert_eq!(rel, Path::new(r".firstcut\report.csv"));
        assert!(strip_album_prefix(root, Path::new(r"F:\Photos\EventOther\a.jpg")).is_none());
    }
}
