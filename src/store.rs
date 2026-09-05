use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};

use crate::model::{MessageRole, NewSession, RuntimeState, SearchHit, Session, Tab, WorkflowState};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const SCHEMA_VERSION: i64 = 2;
const MAX_TITLE_CHARS: usize = 80;

const MIGRATION_1: &str = r"
CREATE TABLE IF NOT EXISTS tabs (
    id              TEXT PRIMARY KEY,
    position        INTEGER NOT NULL,
    title           TEXT NOT NULL,
    layout          TEXT,
    tmux_window_id  TEXT,
    workflow_state  TEXT NOT NULL DEFAULT 'open'
                    CHECK (workflow_state IN ('open', 'completed')),
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    completed_at    INTEGER
);

CREATE UNIQUE INDEX IF NOT EXISTS tabs_tmux_window_id
    ON tabs(tmux_window_id)
    WHERE tmux_window_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS sessions (
    id                TEXT PRIMARY KEY,
    tab_id            TEXT NOT NULL REFERENCES tabs(id),
    position          INTEGER NOT NULL,
    agent             TEXT NOT NULL,
    agent_session_id  TEXT,
    cwd               BLOB NOT NULL,
    title             TEXT NOT NULL,
    transcript_path   BLOB,
    workflow_state    TEXT NOT NULL DEFAULT 'open'
                      CHECK (workflow_state IN ('open', 'completed')),
    runtime_state     TEXT NOT NULL DEFAULT 'creating'
                      CHECK (runtime_state IN ('creating', 'running', 'exited', 'missing')),
    tmux_pane_id      TEXT,
    exit_code         INTEGER,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    completed_at      INTEGER
);

CREATE INDEX IF NOT EXISTS sessions_tab_position
    ON sessions(tab_id, position, id);
CREATE UNIQUE INDEX IF NOT EXISTS sessions_tmux_pane_id
    ON sessions(tmux_pane_id)
    WHERE tmux_pane_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS sessions_agent_identity
    ON sessions(agent, agent_session_id)
    WHERE agent_session_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS messages (
    id          INTEGER PRIMARY KEY,
    session_id  TEXT NOT NULL REFERENCES sessions(id),
    message_id  TEXT NOT NULL,
    role        TEXT NOT NULL CHECK (role IN ('user', 'assistant')),
    content     TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    UNIQUE(session_id, message_id)
);

CREATE INDEX IF NOT EXISTS messages_session_updated
    ON messages(session_id, updated_at, id);

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    content,
    content = 'messages',
    content_rowid = 'id',
    tokenize = 'unicode61'
);

CREATE TRIGGER IF NOT EXISTS messages_fts_insert
AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
END;

CREATE TRIGGER IF NOT EXISTS messages_fts_delete
AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, content)
    VALUES ('delete', old.id, old.content);
END;

CREATE TRIGGER IF NOT EXISTS messages_fts_update
AFTER UPDATE OF content ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, content)
    VALUES ('delete', old.id, old.content);
    INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
END;

INSERT INTO messages_fts(messages_fts) VALUES ('rebuild');
";

const MIGRATION_2: &str = r"
ALTER TABLE sessions
ADD COLUMN provider_ready INTEGER NOT NULL DEFAULT 0
CHECK (provider_ready IN (0, 1));
";

pub(crate) struct Store {
    connection: Connection,
}

impl Store {
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self> {
        prepare_database_file(path.as_ref())?;
        let connection = Connection::open(path.as_ref())
            .with_context(|| format!("open database {}", path.as_ref().display()))?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .context("configure SQLite busy timeout")?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .context("enable SQLite foreign keys")?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .context("enable SQLite WAL mode")?;

        migrate(&connection)?;
        Ok(Self { connection })
    }

    pub(crate) fn create_or_update_tab(&self, id: &str, position: i64, title: &str) -> Result<()> {
        let now = now_timestamp()?;
        self.connection
            .execute(
                r"
                INSERT INTO tabs (
                    id, position, title, workflow_state, created_at, updated_at
                ) VALUES (?1, ?2, ?3, 'open', ?4, ?4)
                ON CONFLICT(id) DO UPDATE SET
                    position = excluded.position,
                    title = excluded.title,
                    updated_at = excluded.updated_at
                ",
                params![id, position, title, now],
            )
            .with_context(|| format!("create or update tab {id}"))?;
        Ok(())
    }

    pub(crate) fn rename_tab(&self, id: &str, title: &str) -> Result<()> {
        let now = now_timestamp()?;
        let changed = self.connection.execute(
            "UPDATE tabs SET title = ?1, updated_at = ?2 \
             WHERE id = ?3 AND workflow_state = 'open'",
            params![title, now, id],
        )?;
        require_one(changed, "open tab", id)
    }

    pub(crate) fn create_session(&self, session: &NewSession) -> Result<()> {
        let now = now_timestamp()?;
        let cwd = encode_path(&session.cwd)?;
        self.connection
            .execute(
                r"
                INSERT INTO sessions (
                    id, tab_id, position, agent, agent_session_id, cwd, title,
                    provider_ready, workflow_state, runtime_state, created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'open', 'creating', ?9, ?9)
                ",
                params![
                    session.id,
                    session.tab_id,
                    session.position,
                    session.agent,
                    session.agent_session_id,
                    cwd,
                    session.title,
                    session.provider_ready,
                    now,
                ],
            )
            .with_context(|| format!("create session {}", session.id))?;
        Ok(())
    }

    pub(crate) fn get_session(&self, id: &str) -> Result<Option<Session>> {
        self.connection
            .query_row(
                &format!("{} WHERE s.id = ?1", session_select()),
                [id],
                session_from_row,
            )
            .optional()
            .with_context(|| format!("get session {id}"))
    }

    pub(crate) fn list_sessions(&self, include_completed: bool) -> Result<Vec<Session>> {
        let sql = format!(
            "{} WHERE ?1 OR (s.workflow_state = 'open' AND t.workflow_state = 'open') \
             ORDER BY t.position, t.id, s.position, s.id",
            session_select()
        );
        let mut statement = self
            .connection
            .prepare(&sql)
            .context("prepare session list")?;
        let sessions = statement
            .query_map([include_completed], session_from_row)
            .context("list sessions")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("decode session list")?;
        Ok(sessions)
    }

    pub(crate) fn list_open_tabs(&self) -> Result<Vec<Tab>> {
        let mut statement = self
            .connection
            .prepare(
                r"
                SELECT id, position, title, layout, tmux_window_id
                FROM tabs
                WHERE workflow_state = 'open'
                ORDER BY position, id
                ",
            )
            .context("prepare open tab list")?;
        let tabs = statement
            .query_map([], |row| {
                Ok(Tab {
                    id: row.get(0)?,
                    position: row.get(1)?,
                    title: row.get(2)?,
                    layout: row.get(3)?,
                    tmux_window_id: row.get(4)?,
                })
            })
            .context("list open tabs")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("decode open tab list")?;
        Ok(tabs)
    }

    pub(crate) fn list_open_sessions(&self) -> Result<Vec<Session>> {
        self.list_sessions(false)
    }

    pub(crate) fn list_open_sessions_for_tab(&self, tab_id: &str) -> Result<Vec<Session>> {
        let sql = format!(
            "{} WHERE s.tab_id = ?1 AND s.workflow_state = 'open' \
             AND t.workflow_state = 'open' ORDER BY s.position, s.id",
            session_select()
        );
        let mut statement = self
            .connection
            .prepare(&sql)
            .context("prepare tab session list")?;
        let sessions = statement
            .query_map([tab_id], session_from_row)
            .with_context(|| format!("list open sessions for tab {tab_id}"))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("decode tab session list")?;
        Ok(sessions)
    }

    pub(crate) fn bind_tab_window(&self, tab_id: &str, window_id: &str) -> Result<()> {
        let now = now_timestamp()?;
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
                .context("start tab binding transaction")?;
        transaction.execute(
            "UPDATE tabs SET tmux_window_id = NULL, updated_at = ?1 \
             WHERE tmux_window_id = ?2 AND id <> ?3",
            params![now, window_id, tab_id],
        )?;
        require_update(
            &transaction,
            "UPDATE tabs SET tmux_window_id = ?1, updated_at = ?2 WHERE id = ?3",
            params![window_id, now, tab_id],
            "tab",
            tab_id,
        )?;
        transaction.commit().context("commit tab binding")?;
        Ok(())
    }

    pub(crate) fn bind_session_pane(&self, session_id: &str, pane_id: &str) -> Result<()> {
        let now = now_timestamp()?;
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
                .context("start pane binding transaction")?;
        release_pane_binding(&transaction, session_id, pane_id, now)?;
        require_update(
            &transaction,
            "UPDATE sessions SET tmux_pane_id = ?1, updated_at = ?2 WHERE id = ?3",
            params![pane_id, now, session_id],
            "session",
            session_id,
        )?;
        transaction.commit().context("commit pane binding")?;
        Ok(())
    }

    pub(crate) fn update_tab_layout_and_positions(
        &self,
        tab_id: &str,
        layout: Option<&str>,
        positions: &[(String, i64)],
    ) -> Result<()> {
        validate_positions(positions)?;
        let now = now_timestamp()?;
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
                .context("start layout transaction")?;
        require_update(
            &transaction,
            "UPDATE tabs SET layout = ?1, updated_at = ?2 WHERE id = ?3",
            params![layout, now, tab_id],
            "tab",
            tab_id,
        )?;
        for (session_id, position) in positions {
            let changed = transaction.execute(
                "UPDATE sessions SET position = ?1, updated_at = ?2 \
                 WHERE id = ?3 AND tab_id = ?4",
                params![position, now, session_id, tab_id],
            )?;
            if changed != 1 {
                bail!("session {session_id} does not belong to tab {tab_id}");
            }
        }
        transaction.commit().context("commit layout update")?;
        Ok(())
    }

    pub(crate) fn register_hook(
        &self,
        session_id: &str,
        agent_session_id: &str,
        transcript_path: Option<&Path>,
        cwd: &Path,
    ) -> Result<()> {
        let now = now_timestamp()?;
        let transcript_path = transcript_path.map(encode_path).transpose()?;
        let cwd = encode_path(cwd)?;
        let changed = self.connection.execute(
            r"
            UPDATE sessions
            SET agent_session_id = ?1,
                transcript_path = COALESCE(?2, transcript_path),
                cwd = ?3,
                provider_ready = 1,
                updated_at = ?4
            WHERE id = ?5
            ",
            params![agent_session_id, transcript_path, cwd, now, session_id],
        )?;
        require_one(changed, "session", session_id)
    }

    pub(crate) fn record_running(&self, session_id: &str, pane_id: &str) -> Result<()> {
        let now = now_timestamp()?;
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
                .context("start runtime transaction")?;
        release_pane_binding(&transaction, session_id, pane_id, now)?;
        require_update(
            &transaction,
            r"
            UPDATE sessions
            SET runtime_state = 'running', tmux_pane_id = ?1,
                exit_code = NULL, updated_at = ?2
            WHERE id = ?3
            ",
            params![pane_id, now, session_id],
            "session",
            session_id,
        )?;
        transaction.commit().context("commit running state")?;
        Ok(())
    }

    pub(crate) fn record_exited(&self, session_id: &str, exit_code: Option<i32>) -> Result<()> {
        let now = now_timestamp()?;
        let changed = self.connection.execute(
            r"
            UPDATE sessions
            SET runtime_state = 'exited', exit_code = ?1, updated_at = ?2
            WHERE id = ?3
            ",
            params![exit_code, now, session_id],
        )?;
        require_one(changed, "session", session_id)
    }

    pub(crate) fn record_missing(&self, session_id: &str) -> Result<()> {
        let now = now_timestamp()?;
        let changed = self.connection.execute(
            r"
            UPDATE sessions
            SET runtime_state = 'missing', tmux_pane_id = NULL,
                exit_code = NULL, updated_at = ?1
            WHERE id = ?2
            ",
            params![now, session_id],
        )?;
        require_one(changed, "session", session_id)
    }

    pub(crate) fn complete_session(&self, session_id: &str) -> Result<()> {
        let now = now_timestamp()?;
        complete_session_record(&self.connection, session_id, now)
    }

    pub(crate) fn complete_pane(&self, session_id: &str) -> Result<()> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
                .context("start pane completion transaction")?;
        let tab_id = transaction
            .query_row(
                "SELECT tab_id FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .with_context(|| format!("session {session_id} does not exist"))?;

        // `complete_session` writes through the same connection and therefore participates in
        // this transaction.
        self.complete_session(session_id)?;
        let open_sessions_remain = transaction.query_row(
            "SELECT EXISTS(\
                 SELECT 1 FROM sessions \
                 WHERE tab_id = ?1 AND workflow_state = 'open'\
             )",
            [&tab_id],
            |row| row.get::<_, bool>(0),
        )?;
        if !open_sessions_remain {
            let now = now_timestamp()?;
            require_update(
                &transaction,
                r"
                UPDATE tabs
                SET workflow_state = 'completed',
                    tmux_window_id = NULL,
                    updated_at = CASE WHEN workflow_state = 'open' THEN ?1 ELSE updated_at END,
                    completed_at = COALESCE(completed_at, ?1)
                WHERE id = ?2
                ",
                params![now, tab_id],
                "tab",
                &tab_id,
            )?;
        }

        transaction.commit().context("commit pane completion")?;
        Ok(())
    }

    pub(crate) fn complete_tab(&self, tab_id: &str) -> Result<()> {
        let now = now_timestamp()?;
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
                .context("start tab completion transaction")?;
        require_update(
            &transaction,
            r"
            UPDATE tabs
            SET workflow_state = 'completed',
                tmux_window_id = NULL,
                updated_at = CASE WHEN workflow_state = 'open' THEN ?1 ELSE updated_at END,
                completed_at = COALESCE(completed_at, ?1)
            WHERE id = ?2
            ",
            params![now, tab_id],
            "tab",
            tab_id,
        )?;
        transaction.execute(
            r"
            UPDATE sessions
            SET workflow_state = 'completed',
                runtime_state = 'exited',
                tmux_pane_id = NULL,
                exit_code = NULL,
                updated_at = CASE WHEN workflow_state = 'open' THEN ?1 ELSE updated_at END,
                completed_at = COALESCE(completed_at, ?1)
            WHERE tab_id = ?2
            ",
            params![now, tab_id],
        )?;
        transaction.commit().context("commit tab completion")?;
        Ok(())
    }

    pub(crate) fn index_message(
        &self,
        session_id: &str,
        role: MessageRole,
        content: &str,
    ) -> Result<()> {
        // Hook payloads do not expose a message ID. Including the complete role/content pair
        // gives exact, process-stable idempotency without a collision-prone hash. Transcript
        // importers that have native IDs should call `index_message_with_id` instead.
        let message_id = format!("implicit\0{}\0{content}", role.as_str());
        self.index_message_with_id(session_id, &message_id, role, content)
    }

    pub(crate) fn index_message_with_id(
        &self,
        session_id: &str,
        message_id: &str,
        role: MessageRole,
        content: &str,
    ) -> Result<()> {
        let now = now_timestamp()?;
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
                .context("start message indexing transaction")?;
        let changed = transaction
            .query_row(
                r"
                INSERT INTO messages (
                    session_id, message_id, role, content, created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                ON CONFLICT(session_id, message_id) DO UPDATE SET
                    role = excluded.role,
                    content = excluded.content,
                    updated_at = excluded.updated_at
                WHERE messages.role IS NOT excluded.role
                   OR messages.content IS NOT excluded.content
                RETURNING id
                ",
                params![session_id, message_id, role.as_str(), content, now],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .with_context(|| format!("index message for session {session_id}"))?;

        if changed.is_some() {
            let title = (role == MessageRole::User)
                .then(|| title_from_prompt(content))
                .flatten();
            let updated = if let Some(title) = title {
                transaction.execute(
                    "UPDATE sessions SET title = ?1, updated_at = ?2 WHERE id = ?3",
                    params![title, now, session_id],
                )?
            } else {
                transaction.execute(
                    "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
                    params![now, session_id],
                )?
            };
            require_one(updated, "session", session_id)?;
        }
        transaction.commit().context("commit message indexing")?;
        Ok(())
    }

    pub(crate) fn search(
        &self,
        query: &str,
        include_completed: bool,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        let Some(query) = literal_fts_query(query) else {
            return Ok(Vec::new());
        };
        let limit = i64::try_from(limit).context("search result limit exceeds SQLite range")?;
        let mut statement = self
            .connection
            .prepare(
                r"
                SELECT s.id,
                       s.agent,
                       s.title,
                       s.cwd,
                       m.role,
                       snippet(messages_fts, 0, '', '', ' … ', 24),
                       m.updated_at,
                       s.workflow_state,
                       s.tmux_pane_id,
                       t.tmux_window_id
                FROM messages_fts
                JOIN messages AS m ON m.id = messages_fts.rowid
                JOIN sessions AS s ON s.id = m.session_id
                JOIN tabs AS t ON t.id = s.tab_id
                WHERE messages_fts MATCH ?1
                  AND (?2 OR (s.workflow_state = 'open' AND t.workflow_state = 'open'))
                ORDER BY bm25(messages_fts), m.updated_at DESC, s.id, m.id
                LIMIT ?3
                ",
            )
            .context("prepare full-text search")?;
        let hits = statement
            .query_map(
                params![query, include_completed, limit],
                search_hit_from_row,
            )
            .context("search messages")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("decode search results")?;
        Ok(hits)
    }

    pub(crate) fn find_session_by_tmux_pane(&self, pane_id: &str) -> Result<Option<Session>> {
        self.connection
            .query_row(
                &format!("{} WHERE s.tmux_pane_id = ?1", session_select()),
                [pane_id],
                session_from_row,
            )
            .optional()
            .with_context(|| format!("find session for tmux pane {pane_id}"))
    }
}

#[cfg(unix)]
fn prepare_database_file(path: &Path) -> Result<()> {
    use std::fs::OpenOptions;
    use std::io::ErrorKind;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    if path == Path::new(":memory:") {
        return Ok(());
    }

    match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => {
            let mut permissions = file
                .metadata()
                .with_context(|| format!("inspect database file {}", path.display()))?
                .permissions();
            permissions.set_mode(0o600);
            file.set_permissions(permissions)
                .with_context(|| format!("secure database file {}", path.display()))?;
            drop(file);
            Ok(())
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("create database file {}", path.display()))
        }
    }
}

#[cfg(not(unix))]
fn prepare_database_file(_path: &Path) -> Result<()> {
    Ok(())
}

fn migrate(connection: &Connection) -> Result<()> {
    loop {
        let version = schema_version(connection)?;
        match version {
            SCHEMA_VERSION => return Ok(()),
            0 => apply_migration(connection, 0, 1, MIGRATION_1)?,
            1 => apply_migration(connection, 1, 2, MIGRATION_2)?,
            version if version > SCHEMA_VERSION => {
                bail!(
                    "database schema version {version} is newer than supported version {SCHEMA_VERSION}"
                );
            }
            version => bail!("no migration from database schema version {version}"),
        }
    }
}

fn schema_version(connection: &Connection) -> Result<i64> {
    connection
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .context("read database schema version")
}

fn apply_migration(
    connection: &Connection,
    expected_version: i64,
    next_version: i64,
    sql: &str,
) -> Result<()> {
    let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
        .with_context(|| format!("start database schema migration {next_version}"))?;

    // Another process may have migrated while this connection waited for the write lock.
    if schema_version(&transaction)? != expected_version {
        transaction
            .rollback()
            .with_context(|| format!("finish concurrent schema migration {next_version}"))?;
        return Ok(());
    }

    transaction
        .execute_batch(sql)
        .with_context(|| format!("apply database schema migration {next_version}"))?;
    transaction
        .pragma_update(None, "user_version", next_version)
        .with_context(|| format!("record database schema migration {next_version}"))?;
    transaction
        .commit()
        .with_context(|| format!("commit database schema migration {next_version}"))
}

fn session_select() -> &'static str {
    r"
    SELECT s.id, s.tab_id, s.position, s.agent, s.agent_session_id, s.cwd,
           s.title, s.transcript_path, s.provider_ready, s.workflow_state, s.runtime_state,
           s.tmux_pane_id, s.exit_code, s.created_at, s.updated_at, s.completed_at
    FROM sessions AS s
    JOIN tabs AS t ON t.id = s.tab_id
    "
}

fn session_from_row(row: &Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        tab_id: row.get(1)?,
        position: row.get(2)?,
        agent: row.get(3)?,
        agent_session_id: row.get(4)?,
        cwd: decode_required_path(row, 5)?,
        title: row.get(6)?,
        transcript_path: decode_optional_path(row, 7)?,
        provider_ready: row.get(8)?,
        workflow_state: parse_workflow_state(&row.get::<_, String>(9)?, 9)?,
        runtime_state: parse_runtime_state(&row.get::<_, String>(10)?, 10)?,
        tmux_pane_id: row.get(11)?,
        exit_code: row.get(12)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
        completed_at: row.get(15)?,
    })
}

fn search_hit_from_row(row: &Row<'_>) -> rusqlite::Result<SearchHit> {
    Ok(SearchHit {
        session_id: row.get(0)?,
        agent: row.get(1)?,
        title: row.get(2)?,
        cwd: decode_required_path(row, 3)?,
        role: parse_message_role(&row.get::<_, String>(4)?, 4)?,
        excerpt: row.get(5)?,
        updated_at: row.get(6)?,
        workflow_state: parse_workflow_state(&row.get::<_, String>(7)?, 7)?,
        tmux_pane_id: row.get(8)?,
        tmux_window_id: row.get(9)?,
    })
}

fn parse_workflow_state(value: &str, column: usize) -> rusqlite::Result<WorkflowState> {
    match value {
        "open" => Ok(WorkflowState::Open),
        "completed" => Ok(WorkflowState::Completed),
        _ => Err(invalid_value(
            column,
            format!("unknown workflow state {value:?}"),
        )),
    }
}

fn parse_runtime_state(value: &str, column: usize) -> rusqlite::Result<RuntimeState> {
    match value {
        "creating" => Ok(RuntimeState::Creating),
        "running" => Ok(RuntimeState::Running),
        "exited" => Ok(RuntimeState::Exited),
        "missing" => Ok(RuntimeState::Missing),
        _ => Err(invalid_value(
            column,
            format!("unknown runtime state {value:?}"),
        )),
    }
}

fn parse_message_role(value: &str, column: usize) -> rusqlite::Result<MessageRole> {
    match value {
        "user" => Ok(MessageRole::User),
        "assistant" => Ok(MessageRole::Assistant),
        _ => Err(invalid_value(
            column,
            format!("unknown message role {value:?}"),
        )),
    }
}

fn invalid_value(column: usize, message: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            message,
        )),
    )
}

fn require_update(
    transaction: &Transaction<'_>,
    sql: &str,
    values: impl rusqlite::Params,
    kind: &str,
    id: &str,
) -> Result<()> {
    let changed = transaction.execute(sql, values)?;
    require_one(changed, kind, id)
}

fn require_one(changed: usize, kind: &str, id: &str) -> Result<()> {
    if changed == 1 {
        Ok(())
    } else {
        Err(anyhow!("{kind} {id} does not exist"))
    }
}

fn complete_session_record(connection: &Connection, session_id: &str, now: i64) -> Result<()> {
    let changed = connection.execute(
        r"
        UPDATE sessions
        SET workflow_state = 'completed',
            runtime_state = 'exited',
            tmux_pane_id = NULL,
            exit_code = NULL,
            updated_at = CASE WHEN workflow_state = 'open' THEN ?1 ELSE updated_at END,
            completed_at = COALESCE(completed_at, ?1)
        WHERE id = ?2
        ",
        params![now, session_id],
    )?;
    require_one(changed, "session", session_id)
}

fn release_pane_binding(
    transaction: &Transaction<'_>,
    session_id: &str,
    pane_id: &str,
    now: i64,
) -> Result<()> {
    transaction.execute(
        r"
        UPDATE sessions
        SET tmux_pane_id = NULL,
            runtime_state = CASE WHEN runtime_state = 'running' THEN 'missing' ELSE runtime_state END,
            updated_at = ?1
        WHERE tmux_pane_id = ?2 AND id <> ?3
        ",
        params![now, pane_id, session_id],
    )?;
    Ok(())
}

fn validate_positions(positions: &[(String, i64)]) -> Result<()> {
    let mut session_ids = HashSet::with_capacity(positions.len());
    let mut position_values = HashSet::with_capacity(positions.len());
    for (session_id, position) in positions {
        if !session_ids.insert(session_id) {
            bail!("duplicate session {session_id} in pane positions");
        }
        if !position_values.insert(position) {
            bail!("duplicate pane position {position}");
        }
    }
    Ok(())
}

fn now_timestamp() -> Result<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?;
    i64::try_from(duration.as_secs()).context("system time exceeds SQLite integer range")
}

fn title_from_prompt(content: &str) -> Option<String> {
    let normalized = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }

    let mut chars = normalized.chars();
    let prefix = chars.by_ref().take(MAX_TITLE_CHARS).collect::<String>();
    if chars.next().is_none() {
        Some(prefix)
    } else {
        let mut truncated = prefix
            .chars()
            .take(MAX_TITLE_CHARS.saturating_sub(1))
            .collect::<String>();
        truncated.push('…');
        Some(truncated)
    }
}

fn literal_fts_query(query: &str) -> Option<String> {
    let terms = query
        .split_whitespace()
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>();
    (!terms.is_empty()).then(|| terms.join(" AND "))
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)] // Kept fallible to share call sites with non-Unix paths.
fn encode_path(path: &Path) -> Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;

    Ok(path.as_os_str().as_bytes().to_vec())
}

#[cfg(not(unix))]
fn encode_path(path: &Path) -> Result<Vec<u8>> {
    path.to_str()
        .map(|path| path.as_bytes().to_vec())
        .ok_or_else(|| anyhow!("path is not valid Unicode: {}", path.display()))
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)] // Kept fallible to share call sites with non-Unix paths.
fn decode_path(bytes: Vec<u8>, column: usize) -> rusqlite::Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    let _ = column;
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(not(unix))]
fn decode_path(bytes: Vec<u8>, column: usize) -> rusqlite::Result<PathBuf> {
    String::from_utf8(bytes)
        .map(PathBuf::from)
        .map_err(|error| invalid_value(column, format!("path is not valid UTF-8: {error}")))
}

fn decode_required_path(row: &Row<'_>, column: usize) -> rusqlite::Result<PathBuf> {
    decode_path(row.get(column)?, column)
}

fn decode_optional_path(row: &Row<'_>, column: usize) -> rusqlite::Result<Option<PathBuf>> {
    row.get::<_, Option<Vec<u8>>>(column)?
        .map(|path| decode_path(path, column))
        .transpose()
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn store() -> Result<(TempDir, Store)> {
        let directory = tempfile::tempdir()?;
        let store = Store::open(directory.path().join("wips.sqlite3"))?;
        Ok((directory, store))
    }

    fn new_session(id: &str, tab_id: &str, position: i64) -> NewSession {
        NewSession {
            id: id.to_owned(),
            tab_id: tab_id.to_owned(),
            position,
            agent: "codex".to_owned(),
            agent_session_id: None,
            cwd: PathBuf::from(format!("/work/{id}")),
            title: "Starting".to_owned(),
            provider_ready: false,
        }
    }

    #[test]
    fn fresh_database_reaches_v2_with_provider_not_ready() -> Result<()> {
        let (_directory, store) = store()?;
        assert_eq!(schema_version(&store.connection)?, 2);

        store.create_or_update_tab("tab", 0, "Work")?;
        store.create_session(&new_session("session", "tab", 0))?;
        let session = store.get_session("session")?.context("missing session")?;
        assert!(!session.provider_ready);
        Ok(())
    }

    #[test]
    fn adopting_a_previous_provider_session_is_stored_ready_to_resume() -> Result<()> {
        let (_directory, store) = store()?;

        store.create_or_update_tab("tab", 0, "Work")?;
        store.create_session(&NewSession {
            agent_session_id: Some("f9b8c7d6-1111-4222-8333-444455556666".to_owned()),
            provider_ready: true,
            ..new_session("session", "tab", 0)
        })?;
        let session = store.get_session("session")?.context("missing session")?;
        assert!(session.provider_ready);
        assert_eq!(
            session.agent_session_id.as_deref(),
            Some("f9b8c7d6-1111-4222-8333-444455556666")
        );
        Ok(())
    }

    #[test]
    fn migrates_v1_sessions_to_provider_not_ready() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let database = directory.path().join("wips.sqlite3");
        let connection = Connection::open(&database)?;
        connection.execute_batch(MIGRATION_1)?;
        connection.pragma_update(None, "user_version", 1)?;
        connection.execute(
            r"
            INSERT INTO tabs (
                id, position, title, workflow_state, created_at, updated_at
            ) VALUES ('tab', 0, 'Work', 'open', 1, 1)
            ",
            [],
        )?;
        connection.execute(
            r"
            INSERT INTO sessions (
                id, tab_id, position, agent, cwd, title,
                workflow_state, runtime_state, created_at, updated_at
            ) VALUES ('session', 'tab', 0, 'codex', ?1, 'Existing',
                      'open', 'exited', 1, 1)
            ",
            [encode_path(Path::new("/work/existing"))?],
        )?;
        drop(connection);

        let store = Store::open(&database)?;
        assert_eq!(schema_version(&store.connection)?, 2);
        let session = store.get_session("session")?.context("missing session")?;
        assert_eq!(session.title, "Existing");
        assert!(!session.provider_ready);
        Ok(())
    }

    #[test]
    fn registering_a_hook_marks_the_provider_ready() -> Result<()> {
        let (_directory, store) = store()?;
        store.create_or_update_tab("tab", 0, "Work")?;
        store.create_session(&new_session("session", "tab", 0))?;
        assert!(
            !store
                .get_session("session")?
                .context("missing session")?
                .provider_ready
        );

        store.register_hook(
            "session",
            "provider-session",
            Some(Path::new("/work/session/transcript.jsonl")),
            Path::new("/work/session"),
        )?;

        let session = store.get_session("session")?.context("missing session")?;
        assert!(session.provider_ready);
        assert_eq!(
            session.agent_session_id.as_deref(),
            Some("provider-session")
        );
        assert_eq!(
            session.transcript_path.as_deref(),
            Some(Path::new("/work/session/transcript.jsonl"))
        );
        Ok(())
    }

    #[test]
    fn lifecycle_keeps_runtime_and_workflow_separate() -> Result<()> {
        let (_directory, store) = store()?;
        store.create_or_update_tab("tab", 0, "Work")?;
        store.create_session(&new_session("session", "tab", 0))?;

        let session = store.get_session("session")?.context("missing session")?;
        assert_eq!(session.workflow_state, WorkflowState::Open);
        assert_eq!(session.runtime_state, RuntimeState::Creating);

        store.register_hook(
            "session",
            "019c6166-2eb2-7000-8000-000000000001",
            Some(Path::new("/work/session/transcript.jsonl")),
            Path::new("/work/session/current"),
        )?;
        store.bind_session_pane("session", "%7")?;
        store.record_running("session", "%7")?;
        assert_eq!(
            store
                .find_session_by_tmux_pane("%7")?
                .map(|session| session.id),
            Some("session".to_owned())
        );

        store.record_exited("session", Some(17))?;
        let exited = store.get_session("session")?.context("missing session")?;
        assert_eq!(exited.runtime_state, RuntimeState::Exited);
        assert_eq!(exited.workflow_state, WorkflowState::Open);
        assert_eq!(exited.exit_code, Some(17));
        assert_eq!(store.list_open_sessions()?.len(), 1);

        store.complete_pane("session")?;
        let completed = store.get_session("session")?.context("missing session")?;
        assert_eq!(completed.workflow_state, WorkflowState::Completed);
        assert_eq!(completed.runtime_state, RuntimeState::Exited);
        assert!(completed.tmux_pane_id.is_none());
        assert!(completed.completed_at.is_some());
        assert!(store.list_open_sessions()?.is_empty());
        Ok(())
    }

    #[test]
    fn completing_the_last_open_pane_completes_and_clears_its_tab() -> Result<()> {
        let (_directory, store) = store()?;
        store.create_or_update_tab("tab", 0, "Work")?;
        store.create_session(&new_session("only", "tab", 0))?;
        store.bind_tab_window("tab", "@1")?;
        store.bind_session_pane("only", "%1")?;

        store.complete_pane("only")?;

        assert!(store.list_open_tabs()?.is_empty());
        assert!(store.list_open_sessions()?.is_empty());
        let (workflow_state, window_id, completed_at) = store.connection.query_row(
            "SELECT workflow_state, tmux_window_id, completed_at FROM tabs WHERE id = 'tab'",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            },
        )?;
        assert_eq!(workflow_state, WorkflowState::Completed.as_str());
        assert!(window_id.is_none());
        assert!(completed_at.is_some());
        Ok(())
    }

    #[test]
    fn completing_one_of_multiple_open_panes_leaves_the_tab_open() -> Result<()> {
        let (_directory, store) = store()?;
        store.create_or_update_tab("tab", 0, "Work")?;
        store.create_session(&new_session("first", "tab", 0))?;
        store.create_session(&new_session("second", "tab", 1))?;
        store.bind_tab_window("tab", "@1")?;
        store.bind_session_pane("first", "%1")?;
        store.bind_session_pane("second", "%2")?;

        store.complete_pane("first")?;

        let tabs = store.list_open_tabs()?;
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].id, "tab");
        assert_eq!(tabs[0].tmux_window_id.as_deref(), Some("@1"));
        let sessions = store.list_open_sessions_for_tab("tab")?;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "second");
        let completed = store
            .get_session("first")?
            .context("missing first session")?;
        assert_eq!(completed.workflow_state, WorkflowState::Completed);
        assert!(completed.tmux_pane_id.is_none());
        Ok(())
    }

    #[test]
    fn restore_lists_tabs_and_sessions_in_position_order() -> Result<()> {
        let (_directory, store) = store()?;
        store.create_or_update_tab("later", 20, "Later")?;
        store.create_or_update_tab("first", 10, "First")?;
        store.create_session(&new_session("one", "first", 5))?;
        store.create_session(&new_session("two", "first", 2))?;
        store.create_session(&new_session("three", "later", 0))?;
        store.bind_tab_window("first", "@4")?;
        store.update_tab_layout_and_positions(
            "first",
            Some("layout-data"),
            &[("one".to_owned(), 0), ("two".to_owned(), 1)],
        )?;

        let tabs = store.list_open_tabs()?;
        assert_eq!(
            tabs.iter().map(|tab| tab.id.as_str()).collect::<Vec<_>>(),
            ["first", "later"]
        );
        assert_eq!(tabs[0].layout.as_deref(), Some("layout-data"));
        assert_eq!(tabs[0].tmux_window_id.as_deref(), Some("@4"));

        let sessions = store.list_open_sessions()?;
        assert_eq!(
            sessions
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            ["one", "two", "three"]
        );
        assert_eq!(store.list_open_sessions_for_tab("first")?.len(), 2);
        Ok(())
    }

    #[test]
    fn renaming_a_tab_preserves_its_runtime_metadata() -> Result<()> {
        let (_directory, store) = store()?;
        store.create_or_update_tab("tab", 0, "Old title")?;
        store.create_session(&new_session("session", "tab", 0))?;
        store.bind_tab_window("tab", "@4")?;
        store.update_tab_layout_and_positions(
            "tab",
            Some("layout-data"),
            &[("session".to_owned(), 0)],
        )?;

        store.rename_tab("tab", "New title")?;

        let tabs = store.list_open_tabs()?;
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].title, "New title");
        assert_eq!(tabs[0].layout.as_deref(), Some("layout-data"));
        assert_eq!(tabs[0].tmux_window_id.as_deref(), Some("@4"));
        Ok(())
    }

    #[test]
    fn message_index_is_idempotent_and_search_uses_literal_and_terms() -> Result<()> {
        let (_directory, store) = store()?;
        store.create_or_update_tab("tab", 0, "Work")?;
        store.create_session(&new_session("session", "tab", 0))?;

        let prompt = "  alpha   beta\nplease explain OR literally  ";
        store.index_message("session", MessageRole::User, prompt)?;
        store.index_message("session", MessageRole::User, prompt)?;
        assert_eq!(
            store
                .connection
                .query_row("SELECT count(*) FROM messages", [], |row| row
                    .get::<_, i64>(0))?,
            1
        );
        assert_eq!(
            store
                .get_session("session")?
                .context("missing session")?
                .title,
            "alpha beta please explain OR literally"
        );

        store.index_message_with_id(
            "session",
            "assistant-1",
            MessageRole::Assistant,
            "alpha alone",
        )?;
        assert_eq!(store.search("alpha beta", false, 20)?.len(), 1);
        assert_eq!(store.search("OR", false, 20)?.len(), 1);
        assert!(store.search("   ", false, 20)?.is_empty());

        store.index_message_with_id(
            "session",
            "assistant-1",
            MessageRole::Assistant,
            "gamma replacement",
        )?;
        assert!(store.search("alpha alone", false, 20)?.is_empty());
        assert_eq!(store.search("gamma", false, 20)?.len(), 1);
        Ok(())
    }

    #[test]
    fn completing_a_tab_soft_deletes_its_sessions_and_search_hits() -> Result<()> {
        let (_directory, store) = store()?;
        store.create_or_update_tab("tab", 0, "Work")?;
        store.create_session(&new_session("session", "tab", 0))?;
        store.index_message("session", MessageRole::User, "remember porcupine")?;

        store.complete_tab("tab")?;
        assert!(store.list_open_tabs()?.is_empty());
        assert!(store.list_open_sessions()?.is_empty());
        assert!(store.search("porcupine", false, 20)?.is_empty());

        let sessions = store.list_sessions(true)?;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].workflow_state, WorkflowState::Completed);
        let hits = store.search("porcupine", true, 20)?;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].workflow_state, WorkflowState::Completed);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn creates_database_with_private_permissions() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir()?;
        let database = directory.path().join("wips.sqlite3");
        let _store = Store::open(&database)?;
        let mode = database.metadata()?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        Ok(())
    }
}
