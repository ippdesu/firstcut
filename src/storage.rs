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

pub struct FileRef {
    parent: Folder,
    name: OsString,
}

impl FileRef {
    /// An output outside the album is honored only when explicitly supplied by
    /// the caller (e.g. --output or --cache). Within the album, every component
    /// below the selected root is opened without following links.
    pub fn at(root: &Path, path: &Path, create_parent: bool) -> io::Result<Self> {
        if let Ok(rel) = path.strip_prefix(root) {
            let (parent, name) = split(rel)?;
            return Folder::open(root)?
                .descend(parent, create_parent)?
                .file(name);
        }
        Self::explicit(path, create_parent)
    }

    /// For data that must stay inside the selected root, such as images and XMP.
    pub fn inside(root: &Path, path: &Path) -> io::Result<Self> {
        let rel = path
            .strip_prefix(root)
            .map_err(|_| denied("路径不在相册内"))?;
        let (parent, name) = split(rel)?;
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

    pub fn write(&self, data: &[u8]) -> io::Result<()> {
        self.replace(|file| file.write_all(data))
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
}
