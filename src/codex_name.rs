use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::paths::Paths;
use crate::store::Store;
use crate::title::normalize_tab_title;
use crate::tmux::Tmux;

const POLL_DELAY: Duration = Duration::from_millis(50);
const NAME_READ_DELAY: Duration = Duration::from_secs(5);

pub(crate) struct CodexNameSync {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl CodexNameSync {
    pub(crate) fn start(paths: Paths, logical_session_id: String, tmux: Tmux) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("wips-codex-name".to_owned())
            .spawn(move || {
                run(&paths, &logical_session_id, &tmux, &worker_stop);
            })
            .ok();
        Self { stop, worker }
    }

    pub(crate) fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for CodexNameSync {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run(paths: &Paths, logical_session_id: &str, tmux: &Tmux, stop: &AtomicBool) {
    let Ok(store) = Store::open(&paths.database) else {
        return;
    };
    let Some(thread_id) = wait_for_thread_id(&store, logical_session_id, stop) else {
        return;
    };

    let codex_home = codex_home();
    let mut last_synced_name = None;
    while !stopped(stop) {
        if let Ok(Some(name)) = read_thread_name(&codex_home, &thread_id) {
            if last_synced_name.as_deref() != Some(name.as_str())
                && sync_tab_title(&store, logical_session_id, &name, tmux).is_ok()
            {
                last_synced_name = Some(name);
            }
        }
        wait_while_running(stop, NAME_READ_DELAY);
    }
}

fn wait_for_thread_id(
    store: &Store,
    logical_session_id: &str,
    stop: &AtomicBool,
) -> Option<String> {
    while !stopped(stop) {
        if let Ok(Some(session)) = store.get_session(logical_session_id) {
            if session.provider_ready {
                return session.agent_session_id.filter(|id| !id.is_empty());
            }
        }
        wait_while_running(stop, POLL_DELAY);
    }
    None
}

fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .unwrap_or_else(|| PathBuf::from(".codex"))
}

fn latest_state_db(codex_home: &Path) -> Result<PathBuf> {
    std::fs::read_dir(codex_home)
        .with_context(|| format!("read Codex home {}", codex_home.display()))?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            let version = name
                .strip_prefix("state_")?
                .strip_suffix(".sqlite")?
                .parse::<u32>()
                .ok()?;
            Some((version, entry.path()))
        })
        .max_by_key(|(version, _)| *version)
        .map(|(_, path)| path)
        .context("find Codex state database")
}

fn read_thread_name(codex_home: &Path, thread_id: &str) -> Result<Option<String>> {
    let path = latest_state_db(codex_home)?;
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("open Codex state database read-only")?;
    let name: Option<Option<String>> = connection
        .query_row(
            "SELECT name FROM threads WHERE id = ?1",
            [thread_id],
            |row| row.get(0),
        )
        .optional()
        .context("read Codex thread name")?;
    Ok(name.flatten().filter(|name| !name.is_empty()))
}

fn sync_tab_title(
    store: &Store,
    logical_session_id: &str,
    provider_title: &str,
    tmux: &Tmux,
) -> Result<()> {
    let title = normalize_tab_title(provider_title)?;
    let Some(tab_id) = store.rename_single_session_tab(logical_session_id, &title)? else {
        return Ok(());
    };
    let (windows, _) = tmux.snapshots()?;
    if let Some(window) = windows.iter().find(|window| window.tab_id == tab_id) {
        tmux.set_window_title(&window.window_id, &title)?;
    }
    Ok(())
}

fn wait_while_running(stop: &AtomicBool, duration: Duration) {
    let slices = duration.as_millis().div_ceil(POLL_DELAY.as_millis());
    for _ in 0..slices {
        if stopped(stop) {
            break;
        }
        thread::sleep(POLL_DELAY);
    }
}

fn stopped(stop: &AtomicBool) -> bool {
    stop.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use rusqlite::Connection;

    use super::read_thread_name;

    #[test]
    fn reads_only_the_named_thread_from_latest_state_database() -> Result<()> {
        let codex_home = tempfile::tempdir()?;
        let old = Connection::open(codex_home.path().join("state_4.sqlite"))?;
        old.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, name TEXT);")?;
        old.execute(
            "INSERT INTO threads (id, name) VALUES ('thread-1', 'Old title')",
            [],
        )?;

        let current = Connection::open(codex_home.path().join("state_5.sqlite"))?;
        current.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, name TEXT);")?;
        current.execute(
            "INSERT INTO threads (id, name) VALUES ('thread-1', 'New title')",
            [],
        )?;
        current.execute(
            "INSERT INTO threads (id, name) VALUES ('thread-2', NULL)",
            [],
        )?;
        current.execute("INSERT INTO threads (id, name) VALUES ('thread-3', '')", [])?;

        assert_eq!(
            read_thread_name(codex_home.path(), "thread-1")?,
            Some("New title".to_owned())
        );
        assert_eq!(read_thread_name(codex_home.path(), "thread-2")?, None);
        assert_eq!(read_thread_name(codex_home.path(), "thread-3")?, None);
        assert_eq!(read_thread_name(codex_home.path(), "missing")?, None);
        Ok(())
    }
}
