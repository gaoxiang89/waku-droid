use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, ensure};
use base64::Engine as _;
use gpui::{Context, PathPromptOptions};
use uuid::Uuid;
use waku_client::{WorkspaceClient, WorkspaceOperation, WorkspaceResult};

use crate::app::Waku;

const CHUNK_BYTES: u64 = 4 * 1024 * 1024;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(120);

impl Waku {
    pub(super) fn download_right_panel_path(
        &mut self,
        relative_path: String,
        name: String,
        is_dir: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.selected_workspace_path().map(Path::to_path_buf) else {
            return;
        };
        if is_dir && let Err(error) = validate_name(&name) {
            self.show_toast(tr!("files.download_failed", error = error.to_string()));
            cx.notify();
            return;
        }
        let daemon = self.selected_daemon().clone();
        let is_local = !daemon.is_remote();
        let workspace = WorkspaceClient::for_daemon(daemon);
        cx.spawn(async move |waku, cx| {
            let destination = if is_dir {
                let receiver = cx.update(|cx| {
                    cx.prompt_for_paths(PathPromptOptions {
                        files: false,
                        directories: true,
                        multiple: false,
                        prompt: Some(tr!("files.download_to").into()),
                    })
                });
                let Ok(Ok(Some(parents))) = receiver.await else {
                    return;
                };
                let Some(parent) = parents.into_iter().next() else {
                    return;
                };
                parent.join(&name)
            } else {
                let directory = dirs::download_dir()
                    .or_else(dirs::home_dir)
                    .unwrap_or_default();
                let receiver = cx.update(|cx| cx.prompt_for_new_path(&directory, Some(&name)));
                let Ok(Ok(Some(destination))) = receiver.await else {
                    return;
                };
                destination
            };
            let Ok((id, cancel)) =
                waku.update(cx, |waku, cx| waku.begin_download(name.clone(), cx))
            else {
                return;
            };
            let (progress, updates) = smol::channel::bounded(1);
            let worker = cx.background_executor().spawn(smol::unblock({
                let destination = destination.clone();
                let cancel = cancel.clone();
                move || {
                    let download = Download {
                        workspace,
                        root,
                        cancel: &cancel,
                    };
                    let mut last_update: Option<Instant> = None;
                    let report = |written, total| {
                        if last_update.is_none_or(|last| last.elapsed() >= PROGRESS_INTERVAL) {
                            last_update = Some(Instant::now());
                            let _ = progress.try_send((written, total));
                        }
                    };
                    if is_dir {
                        download.save_folder(&relative_path, &destination, is_local, report)
                    } else {
                        download.save_file(&relative_path, &destination, report)
                    }
                }
            }));
            while let Ok((written, total)) = updates.recv().await {
                if waku
                    .update(cx, |waku, cx| {
                        waku.update_download_progress(id, written, total, cx)
                    })
                    .is_err()
                {
                    cancel.store(true, Ordering::Release);
                    break;
                }
            }
            let result = worker.await;
            let _ = waku.update(cx, |waku, cx| {
                waku.end_download(id, cx);
                match result {
                    Ok(()) => waku.show_success_toast(tr!(
                        "files.download_saved",
                        name = name,
                        path = destination.display()
                    )),
                    Err(_) if cancel.load(Ordering::Acquire) => {
                        waku.show_toast(tr!("files.download_canceled", name = name))
                    }
                    Err(error) => {
                        waku.show_toast(tr!("files.download_failed", error = format!("{error:#}")))
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

struct Download<'a> {
    workspace: WorkspaceClient,
    root: PathBuf,
    cancel: &'a AtomicBool,
}

struct FolderEntry {
    source: String,
    destination: PathBuf,
    file_bytes: Option<u64>,
}

impl Download<'_> {
    fn check_canceled(&self) -> anyhow::Result<()> {
        ensure!(!self.cancel.load(Ordering::Acquire), "download canceled");
        Ok(())
    }

    fn save_folder(
        &self,
        source: &str,
        destination: &Path,
        is_local: bool,
        mut report: impl FnMut(u64, u64),
    ) -> anyhow::Result<()> {
        self.check_canceled()?;
        ensure!(
            fs::symlink_metadata(destination).is_err(),
            "a file or folder already exists at {}",
            destination.display()
        );
        if is_local {
            let parent = destination.parent().context("missing download directory")?;
            ensure!(
                !parent
                    .canonicalize()?
                    .starts_with(self.root.join(source).canonicalize()?),
                "cannot download a folder into itself"
            );
        }
        let (entries, total) = self.plan_folder(source)?;
        self.check_canceled()?;
        // Creating the root exclusively keeps other downloads and existing folders intact.
        fs::create_dir(destination)
            .with_context(|| format!("could not create {}", destination.display()))?;

        let result = (|| {
            let mut written = 0;
            for entry in entries {
                self.check_canceled()?;
                let target = destination.join(entry.destination);
                if let Some(size) = entry.file_bytes {
                    self.write_file(
                        &entry.source,
                        File::create_new(&target)?,
                        Some(size),
                        |offset, _| {
                            report(written + offset, total);
                        },
                    )
                    .with_context(|| format!("could not download {}", entry.source))?;
                    written += size;
                } else {
                    fs::create_dir(&target)
                        .with_context(|| format!("could not create {}", target.display()))?;
                }
            }
            self.check_canceled()
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(destination);
        }
        result
    }

    fn plan_folder(&self, source: &str) -> anyhow::Result<(Vec<FolderEntry>, u64)> {
        let mut pending = vec![(source.to_owned(), PathBuf::new())];
        let mut plan = Vec::new();
        let mut total = 0u64;
        while let Some((directory, local_path)) = pending.pop() {
            self.check_canceled()?;
            let WorkspaceResult::DownloadDirectory { entries } = self
                .workspace
                .request(WorkspaceOperation::ListDownloadDirectory {
                    root: self.root.clone(),
                    relative_path: PathBuf::from(&directory),
                })
                .with_context(|| format!("could not list {directory}"))?
            else {
                anyhow::bail!("the daemon returned an invalid directory response");
            };
            #[cfg(any(windows, target_os = "macos"))]
            {
                let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
                if let Some((a, b)) = case_collision(&names) {
                    anyhow::bail!(
                        "\"{a}\" and \"{b}\" differ only in letter case and cannot both be saved here"
                    );
                }
            }
            for entry in entries {
                self.check_canceled()?;
                validate_name(&entry.name)?;
                // The destination uses local path rules. Keep the source in the
                // daemon's path syntax (both POSIX and Windows accept a slash).
                let source = format!("{directory}/{}", entry.name);
                let destination = local_path.join(&entry.name);
                if let Some(size) = entry.file_bytes {
                    total = total
                        .checked_add(size)
                        .context("download size exceeds the supported limit")?;
                } else {
                    pending.push((source.clone(), destination.clone()));
                }
                plan.push(FolderEntry {
                    source,
                    destination,
                    file_bytes: entry.file_bytes,
                });
            }
        }
        Ok((plan, total))
    }

    fn save_file(
        &self,
        source: &str,
        destination: &Path,
        report: impl FnMut(u64, u64),
    ) -> anyhow::Result<()> {
        self.check_canceled()?;
        let temporary =
            destination.with_file_name(format!(".narel-download-{}.partial", Uuid::new_v4()));
        let file = File::create_new(&temporary)?;
        let result = self.write_file(source, file, None, report).and_then(|()| {
            self.check_canceled()?;
            Ok(fs::rename(&temporary, destination)?)
        });
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }

    fn write_file(
        &self,
        source: &str,
        mut file: File,
        expected_size: Option<u64>,
        mut report: impl FnMut(u64, u64),
    ) -> anyhow::Result<()> {
        let mut offset = 0;
        let mut size = expected_size;
        loop {
            self.check_canceled()?;
            let root = self.root.clone();
            let relative_path = PathBuf::from(source);
            let length = CHUNK_BYTES;
            let operation = if expected_size.is_some() {
                WorkspaceOperation::ReadDownloadFileChunk {
                    root,
                    relative_path,
                    offset,
                    length,
                }
            } else {
                WorkspaceOperation::ReadFileChunk {
                    root,
                    relative_path,
                    offset,
                    length,
                }
            };
            let WorkspaceResult::FileChunk { bytes, total_bytes } =
                self.workspace.request(operation)?
            else {
                anyhow::bail!("the daemon returned an invalid file response");
            };
            ensure!(
                *size.get_or_insert(total_bytes) == total_bytes,
                "the file changed size while downloading"
            );
            let bytes = base64::engine::general_purpose::STANDARD.decode(bytes.trim())?;
            let count = bytes.len() as u64;
            ensure!(
                count <= (total_bytes - offset).min(length),
                "the daemon returned an invalid chunk size"
            );
            ensure!(
                count > 0 || offset == total_bytes,
                "the file ended early while downloading"
            );
            self.check_canceled()?;
            file.write_all(&bytes)?;
            offset += count;
            report(offset, total_bytes);
            if offset == total_bytes {
                return Ok(());
            }
        }
    }
}

#[cfg_attr(not(any(windows, target_os = "macos", test)), allow(dead_code))]
fn case_collision(names: &[&str]) -> Option<(String, String)> {
    let mut seen = HashMap::with_capacity(names.len());
    for &name in names {
        if let Some(first) = seen.insert(name.to_lowercase(), name) {
            return Some((first.to_owned(), name.to_owned()));
        }
    }
    None
}

#[cfg(windows)]
fn is_reserved_windows_name(name: &str) -> bool {
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem = name.split_once('.').map_or(name, |(stem, _)| stem);
    RESERVED
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
}

fn validate_name(name: &str) -> anyhow::Result<()> {
    let valid = Path::new(name).file_name().is_some_and(|part| part == name);
    #[cfg(windows)]
    let valid = valid
        && !name.contains(['<', '>', ':', '"', '|', '?', '*'])
        && !name.ends_with([' ', '.'])
        && !name.chars().any(char::is_control)
        && !is_reserved_windows_name(name);
    ensure!(
        valid,
        "cannot save \"{name}\" as a local file or folder name"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_collision_detects_case_only_siblings() {
        assert_eq!(
            case_collision(&["README", "src", "readme"]),
            Some(("README".to_owned(), "readme".to_owned()))
        );
    }

    #[test]
    fn case_collision_returns_none_for_distinct_names() {
        assert_eq!(case_collision(&["README", "Cargo.toml", "src"]), None);
        assert_eq!(case_collision(&[]), None);
    }

    #[cfg(windows)]
    #[test]
    fn validate_name_rejects_windows_reserved_names() {
        for name in ["CON", "con.txt", "Lpt1.log", "PRN", "aux.md", "nul", "COM9"] {
            assert!(validate_name(name).is_err(), "expected {name} to fail");
        }
    }

    #[cfg(windows)]
    #[test]
    fn validate_name_accepts_non_reserved_names() {
        for name in ["console.txt", "com10", "lpt0", "my.con"] {
            assert!(validate_name(name).is_ok(), "expected {name} to pass");
        }
    }
}
