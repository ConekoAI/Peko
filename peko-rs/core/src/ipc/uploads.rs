//! Files a service sends in over the HTTP door (ADR-121 D5).
//!
//! `POST /v1/files?name=` writes the body to
//! `$PEKO_HOME/run/uploads/<id>/<name>` and answers `upload:<id>`. A send
//! names it in `attachments`; [`resolve`] turns it back into that path,
//! the send copies it into the peko's workspace like any attachment
//! (ADR-088 D2), and [`remove`] deletes the upload. An upload no send
//! used is deleted after [`TTL`]; all are deleted when the engine starts.
//! Uploads waiting at one time are capped at [`MAX_PENDING_BYTES`] so the
//! door cannot fill the disk.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::io::AsyncWriteExt;

use crate::principal::attachments::{safe_file_name, MAX_FILE_BYTES};

/// What a send names an upload by.
pub const PREFIX: &str = "upload:";
/// An upload no send has used is deleted after this long.
pub const TTL: Duration = Duration::from_hours(1);
/// All uploads waiting at one time, together.
pub const MAX_PENDING_BYTES: u64 = 256 * 1024 * 1024;

/// `$PEKO_HOME/run/uploads`.
#[must_use]
pub fn default_dir() -> PathBuf {
    crate::common::paths::default_run_dir().join("uploads")
}

/// Delete every upload: the engine is starting.
pub fn clear(dir: &Path) {
    if let Err(e) = std::fs::remove_dir_all(dir) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!("could not clear {}: {e}", dir.display());
        }
    }
}

fn is_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The file an `upload:<id>` names, if it is still waiting.
fn file_of(dir: &Path, id: &str) -> Option<PathBuf> {
    if !is_id(id) {
        return None;
    }
    std::fs::read_dir(dir.join(id))
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.is_file() && p.extension().is_none_or(|ext| ext != "part"))
}

/// Replace every `upload:<id>` in a send's attachments with the path of
/// its file. Returns the paths, and the ids to [`remove`] once the send
/// has copied them. The error is the caller-facing reason.
pub fn resolve(dir: &Path, attachments: &[String]) -> Result<(Vec<String>, Vec<String>), String> {
    let mut paths = Vec::with_capacity(attachments.len());
    let mut used = Vec::new();
    for entry in attachments {
        match entry.strip_prefix(PREFIX) {
            Some(id) => {
                let file = file_of(dir, id).ok_or_else(|| {
                    format!("{entry} is not waiting (unknown, already used, or expired)")
                })?;
                paths.push(file.display().to_string());
                used.push(id.to_string());
            }
            None => paths.push(entry.clone()),
        }
    }
    Ok((paths, used))
}

/// Delete uploads a send has copied.
pub fn remove(dir: &Path, ids: &[String]) {
    for id in ids.iter().filter(|id| is_id(id)) {
        let _ = std::fs::remove_dir_all(dir.join(id));
    }
}

/// Why an upload was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// Larger than one attachment may be.
    TooLarge,
    /// The uploads waiting would pass [`MAX_PENDING_BYTES`].
    Full,
    /// No usable file name.
    BadName,
    /// The body ended early or the disk failed.
    Failed(String),
}

/// A finished upload.
#[derive(Debug)]
pub struct Uploaded {
    pub id: String,
    pub name: String,
    pub size: u64,
}

/// The upload folder, with the bytes waiting in it counted.
pub struct Uploads {
    dir: PathBuf,
    max_file: u64,
    max_pending: u64,
    ttl: Duration,
    counted: std::sync::Mutex<Counted>,
}

/// Bytes counted against the folder's total. `finished` can only run
/// high (a send or the sweep removed files since); a refusal re-reads
/// the folder before it stands.
#[derive(Default)]
struct Counted {
    finished: u64,
    in_flight: u64,
}

impl Uploads {
    #[must_use]
    pub fn new(dir: PathBuf) -> Arc<Self> {
        Arc::new(Self::with_limits(
            dir,
            MAX_FILE_BYTES,
            MAX_PENDING_BYTES,
            TTL,
        ))
    }

    #[must_use]
    pub fn with_limits(dir: PathBuf, max_file: u64, max_pending: u64, ttl: Duration) -> Self {
        Self {
            dir,
            max_file,
            max_pending,
            ttl,
            counted: std::sync::Mutex::default(),
        }
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn counted(&self) -> std::sync::MutexGuard<'_, Counted> {
        self.counted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Reserve `len` more bytes for an upload in progress, or refuse.
    fn reserve(&self, len: u64) -> bool {
        let mut counted = self.counted();
        if counted.finished + counted.in_flight + len > self.max_pending {
            counted.finished = self.finished_bytes();
        }
        if counted.finished + counted.in_flight + len > self.max_pending {
            return false;
        }
        counted.in_flight += len;
        true
    }

    /// An upload of `size` reserved bytes ended: finished, or not.
    fn settle(&self, size: u64, finished: bool) {
        let mut counted = self.counted();
        counted.in_flight = counted.in_flight.saturating_sub(size);
        if finished {
            counted.finished += size;
        }
    }

    /// Bytes in every finished upload waiting now, after deleting
    /// expired ones.
    fn finished_bytes(&self) -> u64 {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return 0;
        };
        let now = SystemTime::now();
        let mut total = 0;
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let expired = entry
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|at| now.duration_since(at).unwrap_or_default() > self.ttl);
            if expired {
                let _ = std::fs::remove_dir_all(&path);
                continue;
            }
            if let Ok(files) = std::fs::read_dir(&path) {
                total += files
                    .filter_map(Result::ok)
                    .filter(|f| f.path().extension().is_none_or(|ext| ext != "part"))
                    .filter_map(|f| f.metadata().ok())
                    .map(|m| m.len())
                    .sum::<u64>();
            }
        }
        total
    }

    /// Delete uploads older than the TTL.
    pub fn sweep(&self) {
        let finished = self.finished_bytes();
        self.counted().finished = finished;
    }

    /// Write one upload from `chunks`, refusing it as soon as it passes
    /// the per-file limit or the folder's total.
    pub async fn accept<S, E>(
        &self,
        name: &str,
        declared: Option<u64>,
        mut chunks: S,
    ) -> Result<Uploaded, Refused>
    where
        S: futures::Stream<Item = Result<bytes::Bytes, E>> + Unpin,
        E: std::fmt::Display,
    {
        use futures::StreamExt;
        let safe = safe_file_name(name.trim());
        if name.trim().is_empty() || safe.is_empty() || safe == "." || safe == ".." {
            return Err(Refused::BadName);
        }
        if declared.is_some_and(|n| n > self.max_file) {
            return Err(Refused::TooLarge);
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        let folder = self.dir.join(&id);
        let part = folder.join(format!("{safe}.part"));
        let mut reserved = 0u64;
        let result: Result<u64, Refused> = async {
            create_private_dir(&self.dir).await?;
            tokio::fs::create_dir(&folder)
                .await
                .map_err(|e| Refused::Failed(e.to_string()))?;
            let mut file = tokio::fs::File::create(&part)
                .await
                .map_err(|e| Refused::Failed(e.to_string()))?;
            let mut size = 0u64;
            while let Some(chunk) = chunks.next().await {
                let chunk = chunk.map_err(|e| Refused::Failed(e.to_string()))?;
                size += chunk.len() as u64;
                if size > self.max_file {
                    return Err(Refused::TooLarge);
                }
                if !self.reserve(chunk.len() as u64) {
                    return Err(Refused::Full);
                }
                reserved += chunk.len() as u64;
                file.write_all(&chunk)
                    .await
                    .map_err(|e| Refused::Failed(e.to_string()))?;
            }
            file.flush()
                .await
                .map_err(|e| Refused::Failed(e.to_string()))?;
            Ok(size)
        }
        .await;
        let result = match result {
            Ok(size) => tokio::fs::rename(&part, folder.join(&safe))
                .await
                .map(|()| size)
                .map_err(|e| Refused::Failed(e.to_string())),
            Err(refused) => Err(refused),
        };
        self.settle(reserved, result.is_ok());
        match result {
            Ok(size) => Ok(Uploaded {
                id,
                name: safe,
                size,
            }),
            Err(refused) => {
                let _ = tokio::fs::remove_dir_all(&folder).await;
                Err(refused)
            }
        }
    }
}

async fn create_private_dir(dir: &Path) -> Result<(), Refused> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| Refused::Failed(e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).await;
    }
    Ok(())
}

/// Whether `path` (already resolved, links followed) is a file in some
/// peko's attachments: `attachments/` of its workspace, or of one of its
/// conversations' own folders (`contacts/<conversation>/attachments/`,
/// ADR-113). `workspaces` are the pekos' workspaces, resolved too.
#[must_use]
pub fn in_attachments(path: &Path, workspaces: &[PathBuf]) -> bool {
    workspaces.iter().any(|ws| {
        let Ok(relative) = path.strip_prefix(ws) else {
            return false;
        };
        let parts: Vec<_> = relative.components().collect();
        let named = |i: usize, name: &str| {
            parts
                .get(i)
                .is_some_and(|c| c.as_os_str() == std::ffi::OsStr::new(name))
        };
        // At least one name after the folder: a file, not the folder.
        (named(0, crate::principal::attachments::DIR) && parts.len() > 1)
            || (named(0, "contacts")
                && named(2, crate::principal::attachments::DIR)
                && parts.len() > 3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks(
        parts: &[&[u8]],
    ) -> impl futures::Stream<Item = Result<bytes::Bytes, String>> + Unpin {
        futures::stream::iter(
            parts
                .iter()
                .map(|p| Ok(bytes::Bytes::copy_from_slice(p)))
                .collect::<Vec<_>>(),
        )
    }

    #[tokio::test]
    async fn an_upload_is_resolved_once_and_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let uploads = Uploads::new(tmp.path().join("uploads"));
        let done = uploads
            .accept("photo.png", None, chunks(&[b"abc", b"def"]))
            .await
            .unwrap();
        assert_eq!((done.name.as_str(), done.size), ("photo.png", 6));

        let named = format!("upload:{}", done.id);
        let (paths, used) =
            resolve(uploads.dir(), &[named.clone(), "/abs/other.txt".into()]).unwrap();
        assert!(paths[0].ends_with("photo.png"), "{paths:?}");
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"abcdef");
        assert_eq!(paths[1], "/abs/other.txt");
        remove(uploads.dir(), &used);
        assert!(resolve(uploads.dir(), &[named]).is_err(), "used up");
        assert!(resolve(uploads.dir(), &["upload:../../etc".into()]).is_err());
    }

    #[tokio::test]
    async fn the_name_is_made_safe_and_needed() {
        let tmp = tempfile::tempdir().unwrap();
        let uploads = Uploads::new(tmp.path().to_path_buf());
        let done = uploads
            .accept("../../evil.sh", None, chunks(&[b"x"]))
            .await
            .unwrap();
        assert!(!done.name.contains('/'), "{}", done.name);
        assert!(tmp.path().join(&done.id).join(&done.name).is_file());
        assert_eq!(
            uploads
                .accept("  ", None, chunks(&[b"x"]))
                .await
                .unwrap_err(),
            Refused::BadName
        );
    }

    #[tokio::test]
    async fn a_file_over_the_limit_is_refused_and_leaves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let uploads = Uploads::with_limits(tmp.path().to_path_buf(), 4, 100, TTL);
        assert_eq!(
            uploads
                .accept("a", None, chunks(&[b"abc", b"de"]))
                .await
                .unwrap_err(),
            Refused::TooLarge
        );
        assert_eq!(
            uploads.accept("a", Some(5), chunks(&[])).await.unwrap_err(),
            Refused::TooLarge
        );
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn uploads_waiting_are_capped_together() {
        let tmp = tempfile::tempdir().unwrap();
        let uploads = Uploads::with_limits(tmp.path().to_path_buf(), 10, 12, TTL);
        uploads
            .accept("a", None, chunks(&[b"0123456789"]))
            .await
            .unwrap();
        assert_eq!(
            uploads
                .accept("b", None, chunks(&[b"012"]))
                .await
                .unwrap_err(),
            Refused::Full
        );
        uploads.accept("c", None, chunks(&[b"01"])).await.unwrap();
    }

    #[tokio::test]
    async fn expired_uploads_are_swept() {
        let tmp = tempfile::tempdir().unwrap();
        let uploads = Uploads::with_limits(tmp.path().to_path_buf(), 10, 100, Duration::ZERO);
        let done = uploads.accept("a", None, chunks(&[b"x"])).await.unwrap();
        std::thread::sleep(Duration::from_millis(20));
        uploads.sweep();
        assert!(resolve(tmp.path(), &[format!("upload:{}", done.id)]).is_err());
    }

    #[test]
    fn clear_deletes_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("uploads");
        std::fs::create_dir_all(dir.join("x")).unwrap();
        clear(&dir);
        assert!(!dir.exists());
        clear(&dir); // nothing there is fine
    }

    #[test]
    fn only_files_in_attachments_folders_are_served() {
        let ws = PathBuf::from("/w/pekos/desk");
        let workspaces = [ws.clone()];
        assert!(in_attachments(
            &ws.join("attachments/2026-11-24/ab-x.png"),
            &workspaces
        ));
        assert!(in_attachments(
            &ws.join("contacts/dm_1/attachments/2026-11-24/ab-x.png"),
            &workspaces
        ));
        assert!(!in_attachments(&ws.join("attachments"), &workspaces));
        assert!(!in_attachments(&ws.join("kb/notes.md"), &workspaces));
        assert!(!in_attachments(
            &ws.join("contacts/dm_1/notes.md"),
            &workspaces
        ));
        assert!(!in_attachments(&ws.join("x/attachments/y"), &workspaces));
        assert!(!in_attachments(
            Path::new("/w/runtime/vault.json"),
            &workspaces
        ));
        assert!(!in_attachments(
            Path::new("/w/pekos/desk2/attachments/a"),
            &workspaces
        ));
    }
}
