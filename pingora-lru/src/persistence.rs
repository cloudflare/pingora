// Copyright 2026 Cloudflare, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use log::{info, warn};
use rand::Rng;
use std::fmt;
use std::fs::{rename, File};
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use tokio::task::JoinError;

/// Summary of a shard save operation.
#[derive(Debug)]
pub struct ShardSaveReport<E> {
    pub saved_shards: usize,
    pub failed_shards: usize,
    pub failures: Vec<ShardSaveFailure<E>>,
}

/// Summary of a shard load operation.
#[derive(Debug)]
pub struct ShardLoadReport<E> {
    pub loaded_shards: usize,
    pub missing_shards: usize,
    pub error_shards: usize,
    pub failures: Vec<ShardLoadFailure<E>>,
    pub temp_cleanup: Result<TempFileCleanupReport, JoinError>,
}

/// Summary of orphaned temp-file cleanup.
#[derive(Debug, Default)]
pub struct TempFileCleanupReport {
    pub removed_files: usize,
    pub failures: Vec<ShardFileError>,
}

/// Fatal setup error before per-shard save can run.
#[derive(Debug)]
pub enum ShardSaveSetupError {
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    Join {
        source: JoinError,
    },
}

/// Fatal error from saving shard payloads.
#[derive(Debug)]
pub enum ShardSaveError<E> {
    Setup(ShardSaveSetupError),
    AllShardsFailed {
        shards: usize,
        failures: Vec<ShardSaveFailure<E>>,
    },
}

/// Per-shard save failure.
#[derive(Debug)]
pub enum ShardSaveFailure<E> {
    Serialize {
        shard: usize,
        source: E,
    },
    Io {
        shard: usize,
        source: ShardFileError,
    },
    Join {
        shard: usize,
        source: JoinError,
    },
}

/// Per-shard load failure.
#[derive(Debug)]
pub enum ShardLoadFailure<E> {
    Io {
        shard: usize,
        source: ShardFileError,
    },
    Join {
        shard: usize,
        source: JoinError,
    },
    Deserialize {
        shard: usize,
        source: E,
    },
}

/// File-level persistence failure with path context.
#[derive(Debug)]
pub enum ShardFileError {
    Create {
        path: PathBuf,
        source: std::io::Error,
    },
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    Flush {
        path: PathBuf,
        source: std::io::Error,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
        source: std::io::Error,
    },
    Open {
        path: PathBuf,
        source: std::io::Error,
    },
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    ReadDir {
        path: PathBuf,
        source: std::io::Error,
    },
    ReadDirEntry {
        path: PathBuf,
        source: std::io::Error,
    },
    Remove {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Save all shard payloads under `dir_path` using `file_name.{shard}`.
///
/// The caller owns shard serialization; this helper owns directory creation,
/// temp-file writes, atomic rename, and per-shard failure isolation.
pub async fn save_shards<E, F>(
    dir_path: &str,
    file_name: &str,
    shards: usize,
    mut serialize_shard: F,
) -> Result<ShardSaveReport<E>, ShardSaveError<E>>
where
    E: fmt::Display,
    F: FnMut(usize) -> Result<Vec<u8>, E>,
{
    let dir_path_buf = PathBuf::from(dir_path);
    tokio::task::spawn_blocking({
        let dir_path = dir_path_buf.clone();
        move || {
            std::fs::create_dir_all(&dir_path).map_err(|source| ShardSaveSetupError::CreateDir {
                path: dir_path,
                source,
            })
        }
    })
    .await
    .map_err(|source| ShardSaveError::Setup(ShardSaveSetupError::Join { source }))?
    .map_err(|e| ShardSaveError::Setup(e))?;

    let mut report = ShardSaveReport {
        saved_shards: 0,
        failed_shards: 0,
        failures: Vec::new(),
    };

    for shard in 0..shards {
        let data = match serialize_shard(shard) {
            Ok(data) => data,
            Err(source) => {
                report.failed_shards += 1;
                report
                    .failures
                    .push(ShardSaveFailure::Serialize { shard, source });
                continue;
            }
        };

        let result = tokio::task::spawn_blocking({
            let dir_path = dir_path_buf.clone();
            let file_name = file_name.to_owned();
            move || write_shard_file(&dir_path, &file_name, shard, &data)
        })
        .await;

        match result {
            Ok(Ok(())) => report.saved_shards += 1,
            Ok(Err(source)) => {
                report.failed_shards += 1;
                report.failures.push(ShardSaveFailure::Io { shard, source });
            }
            Err(source) => {
                report.failed_shards += 1;
                report
                    .failures
                    .push(ShardSaveFailure::Join { shard, source });
            }
        }
    }

    log_save_report(shards, &report);

    if report.failed_shards == shards {
        return Err(ShardSaveError::AllShardsFailed {
            shards,
            failures: report.failures,
        });
    }

    Ok(report)
}

/// Save asynchronously serialized shard payloads under `dir_path` using
/// `file_name.{shard}`.
///
/// This is the asynchronous counterpart to [`save_shards`]. Shards are still
/// serialized and written one at a time to bound memory usage.
pub async fn save_shards_async<E, F, Fut>(
    dir_path: &str,
    file_name: &str,
    shards: usize,
    mut serialize_shard: F,
) -> Result<ShardSaveReport<E>, ShardSaveError<E>>
where
    E: fmt::Display,
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = Result<Vec<u8>, E>>,
{
    let dir_path_buf = PathBuf::from(dir_path);
    tokio::task::spawn_blocking({
        let dir_path = dir_path_buf.clone();
        move || {
            std::fs::create_dir_all(&dir_path).map_err(|source| ShardSaveSetupError::CreateDir {
                path: dir_path,
                source,
            })
        }
    })
    .await
    .map_err(|source| ShardSaveError::Setup(ShardSaveSetupError::Join { source }))?
    .map_err(ShardSaveError::Setup)?;

    let mut report = ShardSaveReport {
        saved_shards: 0,
        failed_shards: 0,
        failures: Vec::new(),
    };

    for shard in 0..shards {
        let data = match serialize_shard(shard).await {
            Ok(data) => data,
            Err(source) => {
                report.failed_shards += 1;
                report
                    .failures
                    .push(ShardSaveFailure::Serialize { shard, source });
                continue;
            }
        };

        let result = tokio::task::spawn_blocking({
            let dir_path = dir_path_buf.clone();
            let file_name = file_name.to_owned();
            move || write_shard_file(&dir_path, &file_name, shard, &data)
        })
        .await;

        match result {
            Ok(Ok(())) => report.saved_shards += 1,
            Ok(Err(source)) => {
                report.failed_shards += 1;
                report.failures.push(ShardSaveFailure::Io { shard, source });
            }
            Err(source) => {
                report.failed_shards += 1;
                report
                    .failures
                    .push(ShardSaveFailure::Join { shard, source });
            }
        }
    }

    log_save_report(shards, &report);

    if report.failed_shards == shards {
        return Err(ShardSaveError::AllShardsFailed {
            shards,
            failures: report.failures,
        });
    }

    Ok(report)
}

/// Load all shard payloads from `dir_path` using `file_name.{shard}`.
///
/// Missing shard files are treated as empty shards. The caller owns shard
/// deserialization; this helper owns file reads, temp-file cleanup, and
/// per-shard failure isolation.
pub async fn load_shards<E, F>(
    dir_path: &str,
    file_name: &str,
    shards: usize,
    mut deserialize_shard: F,
) -> ShardLoadReport<E>
where
    E: fmt::Display,
    F: FnMut(usize, &[u8]) -> Result<(), E>,
{
    let dir_path_buf = PathBuf::from(dir_path);
    let mut report = ShardLoadReport {
        loaded_shards: 0,
        missing_shards: 0,
        error_shards: 0,
        failures: Vec::new(),
        temp_cleanup: Ok(TempFileCleanupReport::default()),
    };

    for shard in 0..shards {
        let result = tokio::task::spawn_blocking({
            let dir_path = dir_path_buf.clone();
            let file_name = file_name.to_owned();
            move || read_shard_file(&dir_path, &file_name, shard)
        })
        .await;

        let data = match result {
            Ok(Ok(Some(data))) => data,
            Ok(Ok(None)) => {
                report.missing_shards += 1;
                continue;
            }
            Ok(Err(source)) => {
                report.error_shards += 1;
                report.failures.push(ShardLoadFailure::Io { shard, source });
                continue;
            }
            Err(source) => {
                report.error_shards += 1;
                report
                    .failures
                    .push(ShardLoadFailure::Join { shard, source });
                continue;
            }
        };

        if let Err(source) = deserialize_shard(shard, &data) {
            report.error_shards += 1;
            report
                .failures
                .push(ShardLoadFailure::Deserialize { shard, source });
            continue;
        }

        report.loaded_shards += 1;
    }

    report.temp_cleanup = cleanup_temp_files(dir_path, file_name).await;
    log_load_report(shards, &report);

    report
}

fn log_save_report<E: fmt::Display>(shards: usize, report: &ShardSaveReport<E>) {
    for failure in &report.failures {
        warn!("{failure}. Skipping shard.");
    }

    if report.failed_shards == 0 {
        info!(
            "Successfully saved {}/{shards} shards.",
            report.saved_shards
        );
    } else {
        warn!(
            "Saved {}/{shards} shards; {} shards failed. Persisted LRU state may be incomplete.",
            report.saved_shards, report.failed_shards
        );
    }
}

fn log_load_report<E: fmt::Display>(shards: usize, report: &ShardLoadReport<E>) {
    for failure in &report.failures {
        warn!("{failure}. Skipping shard.");
    }

    match &report.temp_cleanup {
        Ok(cleanup) => {
            for failure in &cleanup.failures {
                warn!("{failure}");
            }
            let failed_files = cleanup.failures.len();
            if cleanup.removed_files > 0 || failed_files > 0 {
                info!(
                    "Temp file cleanup completed. Removed: {}, Errors: {}",
                    cleanup.removed_files, failed_files
                );
            }
        }
        Err(e) => {
            warn!("Temp file cleanup failed: async blocking IO failure {e}");
        }
    }

    if report.loaded_shards == shards {
        info!(
            "Successfully loaded {}/{shards} shards.",
            report.loaded_shards
        );
    } else if report.loaded_shards == 0 && report.error_shards == 0 {
        info!("No persisted LRU shards found. LRU will start empty.");
    } else {
        warn!(
            "Loaded {}/{shards} shards (missing: {}, errored: {}). LRU state may be incomplete.",
            report.loaded_shards, report.missing_shards, report.error_shards
        );
    }
}

async fn cleanup_temp_files(
    dir_path: &str,
    file_name: &str,
) -> Result<TempFileCleanupReport, JoinError> {
    tokio::task::spawn_blocking({
        let dir_path = PathBuf::from(dir_path);
        let file_name = file_name.to_owned();
        move || cleanup_temp_files_blocking(&dir_path, &file_name)
    })
    .await
}

fn write_shard_file(
    dir_path: &Path,
    file_name: &str,
    shard: usize,
    data: &[u8],
) -> Result<(), ShardFileError> {
    let final_path = dir_path.join(format!("{file_name}.{shard}"));
    let random_suffix: u32 = rand::thread_rng().gen();
    let temp_path = dir_path.join(format!("{file_name}.{shard}.{random_suffix:08x}.tmp"));
    let mut file = File::create(&temp_path).map_err(|source| ShardFileError::Create {
        path: temp_path.clone(),
        source,
    })?;
    file.write_all(data)
        .map_err(|source| ShardFileError::Write {
            path: temp_path.clone(),
            source,
        })?;
    file.flush().map_err(|source| ShardFileError::Flush {
        path: temp_path.clone(),
        source,
    })?;
    rename(&temp_path, &final_path).map_err(|source| ShardFileError::Rename {
        from: temp_path,
        to: final_path,
        source,
    })
}

fn read_shard_file(
    dir_path: &Path,
    file_name: &str,
    shard: usize,
) -> Result<Option<Vec<u8>>, ShardFileError> {
    let path = dir_path.join(format!("{file_name}.{shard}"));
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ShardFileError::Open { path, source });
        }
    };
    let mut buffer = Vec::with_capacity(8192);
    file.read_to_end(&mut buffer)
        .map_err(|source| ShardFileError::Read {
            path: path.clone(),
            source,
        })?;
    Ok(Some(buffer))
}

fn cleanup_temp_files_blocking(dir_path: &Path, file_name: &str) -> TempFileCleanupReport {
    let mut report = TempFileCleanupReport::default();

    if !dir_path.exists() {
        return report;
    }

    let entries = match std::fs::read_dir(dir_path) {
        Ok(entries) => entries,
        Err(source) => {
            report.failures.push(ShardFileError::ReadDir {
                path: dir_path.to_owned(),
                source,
            });
            return report;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(source) => {
                report.failures.push(ShardFileError::ReadDirEntry {
                    path: dir_path.to_owned(),
                    source,
                });
                continue;
            }
        };

        let file_name_str = entry.file_name().to_string_lossy().into_owned();
        if file_name_str.starts_with(file_name) && file_name_str.ends_with(".tmp") {
            let path = entry.path();
            match std::fs::remove_file(&path) {
                Ok(()) => report.removed_files += 1,
                Err(source) => {
                    report
                        .failures
                        .push(ShardFileError::Remove { path, source });
                }
            }
        }
    }

    report
}

impl fmt::Display for ShardSaveSetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateDir { path, source } => {
                write!(f, "failed to create {}: {source}", path.display())
            }
            Self::Join { source } => write!(f, "async blocking IO failure: {source}"),
        }
    }
}

impl std::error::Error for ShardSaveSetupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CreateDir { source, .. } => Some(source),
            Self::Join { source } => Some(source),
        }
    }
}

impl<E: fmt::Display> fmt::Display for ShardSaveError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Setup(source) => source.fmt(f),
            Self::AllShardsFailed { shards, .. } => write!(
                f,
                "All {shards} shards failed to save; see prior warnings for per-shard causes."
            ),
        }
    }
}

impl<E> std::error::Error for ShardSaveError<E>
where
    E: fmt::Debug + fmt::Display + std::error::Error + Send + Sync + 'static,
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Setup(source) => Some(source),
            Self::AllShardsFailed { .. } => None,
        }
    }
}

impl<E: fmt::Display> fmt::Display for ShardSaveFailure<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Serialize { shard, source } => {
                write!(f, "failed to serialize shard {shard}: {source}")
            }
            Self::Io { shard, source } => write!(f, "failed to save shard {shard}: {source}"),
            Self::Join { shard, source } => {
                write!(
                    f,
                    "failed to save shard {shard}: async blocking IO failure {source}"
                )
            }
        }
    }
}

impl<E: fmt::Display> fmt::Display for ShardLoadFailure<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { shard, source } => write!(f, "failed to load shard {shard}: {source}"),
            Self::Join { shard, source } => {
                write!(
                    f,
                    "failed to load shard {shard}: async blocking IO failure {source}"
                )
            }
            Self::Deserialize { shard, source } => {
                write!(f, "failed to deserialize shard {shard}: {source}")
            }
        }
    }
}

impl fmt::Display for ShardFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Create { path, source } => {
                write!(f, "failed to create {}: {source}", path.display())
            }
            Self::Write { path, source } => {
                write!(f, "failed to write to {}: {source}", path.display())
            }
            Self::Flush { path, source } => {
                write!(f, "failed to flush temp file {}: {source}", path.display())
            }
            Self::Rename { from, to, source } => write!(
                f,
                "failed to rename file from {} to {}: {source}",
                from.display(),
                to.display()
            ),
            Self::Open { path, source } => {
                write!(f, "failed to open {}: {source}", path.display())
            }
            Self::Read { path, source } => {
                write!(f, "failed to read from {}: {source}", path.display())
            }
            Self::ReadDir { path, source } => {
                write!(f, "failed to read directory {}: {source}", path.display())
            }
            Self::ReadDirEntry { path, source } => write!(
                f,
                "failed to read directory entry in {}: {source}",
                path.display()
            ),
            Self::Remove { path, source } => {
                write!(f, "failed to remove temp file {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for ShardFileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Create { source, .. }
            | Self::Write { source, .. }
            | Self::Flush { source, .. }
            | Self::Rename { source, .. }
            | Self::Open { source, .. }
            | Self::Read { source, .. }
            | Self::ReadDir { source, .. }
            | Self::ReadDirEntry { source, .. }
            | Self::Remove { source, .. } => Some(source),
        }
    }
}
