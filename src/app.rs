use std::env;
use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use uuid::Uuid;

use crate::config::{AgentKind, Config};
use crate::hook;
use crate::model::{NewSession, RuntimeState, Session, Tab, WorkflowState};
use crate::paths::Paths;
use crate::provider::{
    LaunchContext, build_new_command, build_resume_command, validate_claude_session_id,
    write_claude_settings,
};
use crate::store::Store;
use crate::tmux::{PaneSnapshot, SplitDirection, Tmux, WindowSnapshot};

#[derive(Debug, Parser)]
#[command(
    name = "wips",
    version,
    about = "Keep work-in-progress agent sessions ready to resume"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Action>,
}

#[derive(Debug, Subcommand)]
enum Action {
    /// Restore every open WIP and attach to the terminal workspace.
    Start,
    /// Create a tab or split running a configured agent.
    New(NewArgs),
    /// List tracked sessions.
    List {
        /// Include sessions that were explicitly completed.
        #[arg(long)]
        all: bool,
    },
    /// Full-text search the prompts and final responses captured by WIPS.
    Search {
        /// Search terms. With no terms, WIPS prompts interactively.
        query: Vec<String>,
    },
    /// Mark a pane or tab complete and close it.
    Close(CloseArgs),
    /// Resume the exited agent in a pane.
    Resume {
        /// tmux pane identifier, such as %3.
        #[arg(long)]
        pane: String,
    },
    /// Check configuration, storage, tmux, and agent executables.
    Doctor,
    #[command(hide = true)]
    Hook,
    #[command(name = "_run", hide = true)]
    Run {
        #[arg(long)]
        session: String,
    },
    #[command(name = "_create", hide = true)]
    Create {
        #[arg(long)]
        agent: Option<String>,
    },
    #[command(name = "_hold", hide = true)]
    Hold,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SplitArg {
    Horizontal,
    Vertical,
}

impl From<SplitArg> for SplitDirection {
    fn from(value: SplitArg) -> Self {
        match value {
            SplitArg::Horizontal => Self::LeftRight,
            SplitArg::Vertical => Self::TopBottom,
        }
    }
}

#[derive(Debug, Args)]
struct NewArgs {
    /// Name of an agent preset from config.toml.
    #[arg(long)]
    agent: Option<String>,
    /// Split an existing tab instead of creating a new tab.
    #[arg(long, value_enum)]
    split: Option<SplitArg>,
    /// Target tmux pane for `--split`; defaults to `TMUX_PANE` inside tmux.
    #[arg(long, requires = "split")]
    target: Option<String>,
    /// Working directory for the agent.
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Adopt a previous agent session by its provider session ID (a Claude
    /// UUID, or a Codex session ID) instead of starting a new one. The
    /// session does not need to have been tracked by WIPS before.
    #[arg(long)]
    resume: Option<String>,
}

#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("target")
        .required(true)
        .multiple(false)
        .args(["pane", "window"])
))]
struct CloseArgs {
    /// tmux pane identifier, such as %3.
    #[arg(long)]
    pane: Option<String>,
    /// tmux window identifier, such as @1.
    #[arg(long)]
    window: Option<String>,
    /// Ask before marking the WIP complete.
    #[arg(long)]
    confirm: bool,
}

pub(crate) fn run() -> Result<u8> {
    let cli = Cli::parse();
    let paths = Paths::discover()?;
    paths.ensure_dirs()?;

    match cli.command.unwrap_or(Action::Start) {
        Action::Start => start(&paths),
        Action::New(args) => create_from_cli(&paths, &args),
        Action::List { all } => list(&paths, all),
        Action::Search { query } => search(&paths, &query),
        Action::Close(args) => close(&paths, &args),
        Action::Resume { pane } => resume(&paths, &pane),
        Action::Doctor => doctor(&paths),
        Action::Hook => {
            hook::handle(&paths)?;
            Ok(0)
        }
        Action::Run { session } => run_session(&paths, &session),
        Action::Create { agent } => create_inside_tmux(&paths, agent.as_deref()),
        Action::Hold => loop {
            std::thread::park();
        },
    }
}

fn start(paths: &Paths) -> Result<u8> {
    let (config, store, tmux) = context(paths)?;
    Tmux::probe().context("tmux is required to run WIPS")?;
    ensure_open_workspace(paths, &config, &store, &tmux)?;
    tmux.configure()?;
    sync_tmux_state(&store, &tmux)?;
    tmux.attach()?;
    if tmux.session_exists()? {
        sync_tmux_state(&store, &tmux)?;
    }
    Ok(0)
}

fn create_from_cli(paths: &Paths, args: &NewArgs) -> Result<u8> {
    let (config, store, tmux) = context(paths)?;
    Tmux::probe().context("tmux is required to create a WIP")?;
    let cwd = resolve_new_cwd(args.cwd.as_deref())?;
    let agent_name = args.agent.as_deref().unwrap_or(&config.default_agent);
    let agent = config.agent(agent_name)?;
    let resume = args.resume.as_deref();
    if let Some(provider_session_id) = resume {
        validate_provider_session_id(agent.kind, provider_session_id)?;
    }

    if let Some(split) = args.split {
        ensure_open_workspace(paths, &config, &store, &tmux)?;
        tmux.configure()?;
        let target = args
            .target
            .clone()
            .or_else(|| env::var("TMUX_PANE").ok())
            .context("--split needs --target when WIPS is invoked outside tmux")?;
        let (window_id, tab_id) = tmux.current_ids(&target)?;
        if tab_id.is_empty() {
            bail!("target pane {target} is not managed by WIPS");
        }
        let position = next_session_position(&store, &tab_id)?;
        let session =
            create_session_record(&store, &config, &tab_id, position, agent_name, cwd, resume)?;
        let created = tmux.split(
            &target,
            split.into(),
            &session.id,
            &session.title,
            launch_cwd(paths, &session),
        )?;
        store.bind_session_pane(&session.id, &created.pane_id)?;
        store.bind_tab_window(&tab_id, &window_id)?;
        sync_tmux_state(&store, &tmux)?;
        println!("created {} in pane {}", session.id, created.pane_id);
    } else {
        if tmux.session_exists()? {
            ensure_open_workspace(paths, &config, &store, &tmux)?;
        }
        let position = next_tab_position(&store)?;
        let tab_id = Uuid::new_v4().to_string();
        let tab = Tab {
            id: tab_id.clone(),
            position,
            title: tab_title(&cwd),
            layout: None,
            tmux_window_id: None,
        };
        store.create_or_update_tab(&tab.id, tab.position, &tab.title)?;
        let session = create_session_record(&store, &config, &tab_id, 0, agent_name, cwd, resume)?;
        let created = if tmux.session_exists()? {
            tmux.new_window(
                &tab_id,
                &session.id,
                &tab.title,
                launch_cwd(paths, &session),
            )?
        } else {
            tmux.create_session(
                &tab_id,
                &session.id,
                &tab.title,
                launch_cwd(paths, &session),
            )?
        };
        store.bind_tab_window(&tab_id, &created.window_id)?;
        store.bind_session_pane(&session.id, &created.pane_id)?;
        tmux.configure()?;
        sync_tmux_state(&store, &tmux)?;
        println!("created {} in pane {}", session.id, created.pane_id);
    }

    Ok(0)
}

fn create_inside_tmux(paths: &Paths, requested_agent: Option<&str>) -> Result<u8> {
    let (config, store, tmux) = context(paths)?;
    let pane_id = env::var("TMUX_PANE").context("_create must run inside a tmux pane")?;
    let cwd = resolve_new_cwd(None)?;
    let agent_name = requested_agent.unwrap_or(&config.default_agent);
    config.agent(agent_name)?;
    let (window_id, existing_tab_id) = tmux.current_ids(&pane_id)?;
    let (windows, _) = tmux.snapshots()?;
    let window = windows
        .iter()
        .find(|window| window.window_id == window_id)
        .context("new tmux pane disappeared before WIPS could register it")?;

    let tab_id = if existing_tab_id.is_empty() {
        let id = Uuid::new_v4().to_string();
        store.create_or_update_tab(&id, window.index, &tab_title(&cwd))?;
        store.bind_tab_window(&id, &window_id)?;
        id
    } else {
        existing_tab_id
    };
    let position = next_session_position(&store, &tab_id)?;
    let session = create_session_record(&store, &config, &tab_id, position, agent_name, cwd, None)?;
    tmux.tag_existing(&window_id, &tab_id, &pane_id, &session.id, &session.title)?;
    store.bind_tab_window(&tab_id, &window_id)?;
    store.bind_session_pane(&session.id, &pane_id)?;
    sync_tmux_state(&store, &tmux)?;
    run_session(paths, &session.id)
}

fn run_session(paths: &Paths, logical_session_id: &str) -> Result<u8> {
    let config = Config::load_or_create(paths)?;
    let store = Store::open(&paths.database)?;
    let session = store
        .get_session(logical_session_id)?
        .with_context(|| format!("session {logical_session_id} does not exist"))?;
    if session.workflow_state != WorkflowState::Open {
        bail!("session {logical_session_id} is completed");
    }
    let agent = config.agent(&session.agent)?;
    let pane_id = env::var("TMUX_PANE")
        .or_else(|_| {
            session
                .tmux_pane_id
                .clone()
                .ok_or(env::VarError::NotPresent)
        })
        .context("agent process has no tmux pane identifier")?;
    let current_exe = env::current_exe().context("resolve the running WIPS executable")?;
    let claude_settings_path = paths
        .state_dir
        .join(format!("claude-hooks-{}.json", session.id));
    let launch = LaunchContext {
        logical_session_id: &session.id,
        current_exe: &current_exe,
        state_dir: &paths.state_dir,
        config_path: &paths.config,
        claude_settings_path: &claude_settings_path,
        tmux_socket: &config.tmux.socket,
        tmux_session: &config.tmux.session,
    };
    if agent.kind == AgentKind::Claude {
        write_claude_settings(&claude_settings_path, &current_exe)?;
    }
    let is_new = should_start_new(session.runtime_state, session.provider_ready);
    let command = if is_new {
        build_new_command(agent, &session, &launch)?
    } else {
        build_resume_command(agent, &session, &launch)?
    };

    store.bind_session_pane(&session.id, &pane_id)?;
    make_tmux(&config)?.set_pane_title(&pane_id, &terminal_text(&session.title, 80))?;
    let mut command_process = Command::new(&command.program);
    command_process
        .args(&command.args)
        .envs(&command.env)
        .current_dir(&command.cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut child = command_process.spawn().with_context(|| {
        format!(
            "launch agent preset `{}` with program {:?}",
            session.agent, command.program
        )
    })?;
    store.record_running(&session.id, &pane_id)?;
    let status = match child.wait() {
        Ok(status) => status,
        Err(error) => {
            store.record_exited(&session.id, None)?;
            return Err(error).with_context(|| {
                format!(
                    "wait for agent preset `{}` with program {:?}",
                    session.agent, command.program
                )
            });
        }
    };
    store.record_exited(&session.id, status.code())?;
    Ok(status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1))
}

fn ensure_open_workspace(paths: &Paths, config: &Config, store: &Store, tmux: &Tmux) -> Result<()> {
    let mut tabs = store.list_open_tabs()?;
    for tab in &tabs {
        if store.list_open_sessions_for_tab(&tab.id)?.is_empty() {
            store.complete_tab(&tab.id)?;
        }
    }
    tabs = store.list_open_tabs()?;

    if tabs.is_empty() {
        let cwd = resolve_new_cwd(None)?;
        let tab = Tab {
            id: Uuid::new_v4().to_string(),
            position: 0,
            title: tab_title(&cwd),
            layout: None,
            tmux_window_id: None,
        };
        store.create_or_update_tab(&tab.id, tab.position, &tab.title)?;
        create_session_record(store, config, &tab.id, 0, &config.default_agent, cwd, None)?;
        tabs.push(tab);
    }

    let mut server_exists = tmux.session_exists()?;
    let (mut windows, mut panes) = if server_exists {
        tmux.snapshots()?
    } else {
        (Vec::new(), Vec::new())
    };

    for tab in tabs {
        let sessions = store.list_open_sessions_for_tab(&tab.id)?;
        if sessions.is_empty() {
            continue;
        }
        let mut window = windows.iter().find(|item| item.tab_id == tab.id).cloned();
        let mut target_pane = window.as_ref().and_then(|item| {
            panes
                .iter()
                .find(|pane| pane.window_id == item.window_id && !pane.session_id.is_empty())
                .map(|pane| pane.pane_id.clone())
        });

        for session in sessions {
            if panes.iter().any(|pane| pane.session_id == session.id) {
                continue;
            }
            let created = if let Some(target) = target_pane.as_deref() {
                tmux.split(
                    target,
                    SplitDirection::LeftRight,
                    &session.id,
                    &session.title,
                    launch_cwd(paths, &session),
                )?
            } else if server_exists {
                tmux.new_window(
                    &tab.id,
                    &session.id,
                    &tab.title,
                    launch_cwd(paths, &session),
                )?
            } else {
                let created = tmux.create_session(
                    &tab.id,
                    &session.id,
                    &tab.title,
                    launch_cwd(paths, &session),
                )?;
                server_exists = true;
                created
            };
            store.bind_tab_window(&tab.id, &created.window_id)?;
            store.bind_session_pane(&session.id, &created.pane_id)?;
            target_pane.get_or_insert_with(|| created.pane_id.clone());
            window = Some(WindowSnapshot {
                window_id: created.window_id.clone(),
                tab_id: tab.id.clone(),
                index: tab.position,
                title: tab.title.clone(),
                layout: String::new(),
            });
            panes.push(PaneSnapshot {
                window_id: created.window_id,
                pane_id: created.pane_id,
                session_id: session.id,
                index: session.position,
                dead: false,
                exit_code: None,
            });
        }

        if let (Some(window), Some(layout)) = (window, tab.layout.as_deref()) {
            tmux.apply_layout(&window.window_id, layout)?;
            tmux.set_window_title(&window.window_id, &tab.title)?;
        }
        windows = tmux.snapshots()?.0;
    }

    sync_tmux_state(store, tmux)
}

fn sync_tmux_state(store: &Store, tmux: &Tmux) -> Result<()> {
    let (windows, panes) = tmux.snapshots()?;
    let live_session_ids = panes
        .iter()
        .filter(|pane| !pane.session_id.is_empty())
        .map(|pane| pane.session_id.as_str())
        .collect::<std::collections::HashSet<_>>();
    for pane in panes
        .iter()
        .filter(|pane| pane.dead && !pane.session_id.is_empty())
    {
        let Some(session) = store.get_session(&pane.session_id)? else {
            continue;
        };
        let exit_code = pane.exit_code.or(session.exit_code);
        if session.workflow_state == WorkflowState::Open
            && (session.runtime_state != RuntimeState::Exited || session.exit_code != exit_code)
        {
            store.record_exited(&session.id, exit_code)?;
        }
    }
    for session in store.list_open_sessions()? {
        if session.tmux_pane_id.is_some() && !live_session_ids.contains(session.id.as_str()) {
            store.record_missing(&session.id)?;
        }
    }
    for window in windows.iter().filter(|window| !window.tab_id.is_empty()) {
        store.bind_tab_window(&window.tab_id, &window.window_id)?;
        let mut window_panes = panes
            .iter()
            .filter(|pane| pane.window_id == window.window_id && !pane.session_id.is_empty())
            .collect::<Vec<_>>();
        window_panes.sort_by_key(|pane| pane.index);
        let positions = window_panes
            .iter()
            .enumerate()
            .map(|(position, pane)| {
                let position = i64::try_from(position).context("too many panes to store")?;
                Ok((pane.session_id.clone(), position))
            })
            .collect::<Result<Vec<_>>>()?;
        for pane in &window_panes {
            store.bind_session_pane(&pane.session_id, &pane.pane_id)?;
        }
        store.update_tab_layout_and_positions(&window.tab_id, Some(&window.layout), &positions)?;
    }
    Ok(())
}

fn close(paths: &Paths, args: &CloseArgs) -> Result<u8> {
    let (_, store, tmux) = context(paths)?;
    if !tmux.session_exists()? {
        bail!("the WIPS tmux session is not running");
    }

    if let Some(pane_id) = args.pane.as_deref() {
        let session = store
            .find_session_by_tmux_pane(pane_id)?
            .with_context(|| format!("pane {pane_id} is not tracked by WIPS"))?;
        if args.confirm
            && !confirm(&format!(
                "Mark '{}' complete and close its pane?",
                terminal_text(&session.title, 80)
            ))?
        {
            return Ok(0);
        }
        store.complete_pane(&session.id)?;
        tmux.kill_pane(pane_id)?;
    } else if let Some(window_id) = args.window.as_deref() {
        let (windows, _) = tmux.snapshots()?;
        let tab = windows
            .iter()
            .find(|window| window.window_id == window_id)
            .filter(|window| !window.tab_id.is_empty())
            .context("tmux window is not managed by WIPS")?;
        if args.confirm
            && !confirm(&format!(
                "Mark tab '{}' and every WIP in it complete?",
                terminal_text(&tab.title, 80)
            ))?
        {
            return Ok(0);
        }
        store.complete_tab(&tab.tab_id)?;
        tmux.kill_window(window_id)?;
    }

    if tmux.session_exists()? {
        sync_tmux_state(&store, &tmux)?;
    }
    Ok(0)
}

fn resume(paths: &Paths, pane_id: &str) -> Result<u8> {
    let (_, store, tmux) = context(paths)?;
    if !tmux.session_exists()? {
        bail!("the WIPS tmux session is not running; run `wips` to restore it");
    }
    let session = store
        .find_session_by_tmux_pane(pane_id)?
        .with_context(|| format!("pane {pane_id} is not tracked by WIPS"))?;
    if session.workflow_state != WorkflowState::Open {
        bail!("session {} is completed", session.id);
    }
    let (_, panes) = tmux.snapshots()?;
    let pane = panes
        .iter()
        .find(|pane| pane.pane_id == pane_id)
        .context("pane is no longer present in tmux")?;
    if !pane.dead {
        bail!("pane {pane_id} is still running; resume is only for exited agents");
    }
    tmux.respawn(pane_id, &session.id, launch_cwd(paths, &session))?;
    Ok(0)
}

fn list(paths: &Paths, include_completed: bool) -> Result<u8> {
    let store = Store::open(&paths.database)?;
    let sessions = if include_completed {
        store.list_sessions(true)?
    } else {
        store.list_open_sessions()?
    };
    if sessions.is_empty() {
        println!(
            "No {}WIPs.",
            if include_completed {
                "tracked "
            } else {
                "open "
            }
        );
        return Ok(0);
    }
    println!("STATE\tRUNTIME\tAGENT\tPANE\tTITLE\tDIRECTORY");
    for session in sessions {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            session.workflow_state.as_str(),
            session.runtime_state.as_str(),
            terminal_text(&session.agent, 24),
            session.tmux_pane_id.as_deref().unwrap_or("-"),
            terminal_text(&session.title, 80),
            terminal_text(&session.cwd.to_string_lossy(), 160),
        );
    }
    Ok(0)
}

fn search(paths: &Paths, words: &[String]) -> Result<u8> {
    let store = Store::open(&paths.database)?;
    let interactive = words.is_empty();
    let query = if interactive {
        prompt("Search: ")?
    } else {
        words.join(" ")
    };
    if query.trim().is_empty() {
        return Ok(0);
    }
    let hits = store.search(&query, true, 50)?;
    if hits.is_empty() {
        println!("No matching WIPs.");
        return Ok(0);
    }
    for (index, hit) in hits.iter().enumerate() {
        println!(
            "{:>2}. [{} / {} / {}] {}\n    {}\n    {}",
            index + 1,
            hit.workflow_state.as_str(),
            hit.agent,
            hit.role.as_str(),
            terminal_text(&hit.title, 100),
            terminal_text(&hit.cwd.to_string_lossy(), 160),
            terminal_text(&hit.excerpt, 240),
        );
    }

    if interactive {
        let choice = prompt("Open result (number, Enter to cancel): ")?;
        if choice.trim().is_empty() {
            return Ok(0);
        }
        let index = choice
            .trim()
            .parse::<usize>()
            .context("selection must be a result number")?;
        let hit = hits
            .get(index.saturating_sub(1))
            .context("selection is outside the result list")?;
        let pane_id = hit
            .tmux_pane_id
            .as_deref()
            .context("that result is not in the current tmux workspace")?;
        let config = Config::load_or_create(paths)?;
        let tmux = make_tmux(&config)?;
        tmux.select_pane(pane_id)?;
    }
    Ok(0)
}

fn doctor(paths: &Paths) -> Result<u8> {
    let config = Config::load_or_create(paths)?;
    Store::open(&paths.database)?;
    println!("ok   config   {}", paths.config.display());
    println!("ok   state    {}", paths.state_dir.display());
    println!("ok   database {}", paths.database.display());
    println!("ok   tmux     {}", Tmux::probe()?);

    let mut default_missing = false;
    for (name, agent) in &config.agents {
        if executable_exists(OsStr::new(&agent.program)) {
            println!("ok   agent    {name} -> {}", agent.program);
        } else {
            println!("warn agent    {name} -> {} (not found)", agent.program);
            default_missing |= name == &config.default_agent;
        }
    }
    println!(
        "note codex    Review and trust WIPS SessionStart/UserPromptSubmit/Stop hooks once with /hooks"
    );
    if default_missing {
        bail!("default agent `{}` is not executable", config.default_agent);
    }
    Ok(0)
}

fn context(paths: &Paths) -> Result<(Config, Store, Tmux)> {
    let config = Config::load_or_create(paths)?;
    let store = Store::open(&paths.database)?;
    let tmux = make_tmux(&config)?;
    Ok((config, store, tmux))
}

fn make_tmux(config: &Config) -> Result<Tmux> {
    Ok(Tmux::new(
        &config.tmux.socket,
        &config.tmux.session,
        env::current_exe().context("resolve the running WIPS executable")?,
    ))
}

fn create_session_record(
    store: &Store,
    config: &Config,
    tab_id: &str,
    position: i64,
    agent_name: &str,
    cwd: PathBuf,
    resume_provider_session_id: Option<&str>,
) -> Result<Session> {
    let agent = config.agent(agent_name)?;
    let (agent_session_id, provider_ready) = match resume_provider_session_id {
        Some(provider_session_id) => (Some(provider_session_id.to_owned()), true),
        None => (
            (agent.kind == AgentKind::Claude).then(|| Uuid::new_v4().to_string()),
            false,
        ),
    };
    let new_session = NewSession {
        id: Uuid::new_v4().to_string(),
        tab_id: tab_id.to_owned(),
        position,
        agent: agent_name.to_owned(),
        agent_session_id,
        title: format!("{agent_name}: {}", tab_title(&cwd)),
        cwd,
        provider_ready,
    };
    store.create_session(&new_session)?;
    store
        .get_session(&new_session.id)?
        .with_context(|| format!("new session {} was not stored", new_session.id))
}

fn validate_provider_session_id(kind: AgentKind, provider_session_id: &str) -> Result<()> {
    if provider_session_id.trim().is_empty() {
        bail!("--resume needs a provider session ID");
    }
    if kind == AgentKind::Claude {
        validate_claude_session_id(provider_session_id)
            .context("--resume expects a Claude session UUID")?;
    }
    Ok(())
}

fn next_tab_position(store: &Store) -> Result<i64> {
    Ok(store
        .list_open_tabs()?
        .iter()
        .map(|tab| tab.position)
        .max()
        .unwrap_or(-1)
        + 1)
}

fn next_session_position(store: &Store, tab_id: &str) -> Result<i64> {
    Ok(store
        .list_open_sessions_for_tab(tab_id)?
        .iter()
        .map(|session| session.position)
        .max()
        .unwrap_or(-1)
        + 1)
}

fn resolve_new_cwd(requested: Option<&Path>) -> Result<PathBuf> {
    let cwd = requested.map_or_else(env::current_dir, |path| Ok(path.to_path_buf()))?;
    if !cwd.is_dir() {
        bail!(
            "working directory does not exist or is not a directory: {}",
            cwd.display()
        );
    }
    cwd.canonicalize()
        .with_context(|| format!("resolve working directory {}", cwd.display()))
}

fn launch_cwd<'a>(paths: &'a Paths, session: &'a Session) -> &'a Path {
    if session.cwd.is_dir() {
        &session.cwd
    } else {
        &paths.state_dir
    }
}

fn should_start_new(runtime_state: RuntimeState, provider_ready: bool) -> bool {
    runtime_state == RuntimeState::Creating && !provider_ready
}

fn tab_title(cwd: &Path) -> String {
    match cwd.file_name().filter(|name| !name.is_empty()) {
        Some(name) => terminal_text(&name.to_string_lossy(), 60),
        None => terminal_text(&cwd.to_string_lossy(), 60),
    }
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush().context("flush terminal prompt")?;
    let mut value = String::new();
    io::stdin()
        .read_line(&mut value)
        .context("read terminal input")?;
    Ok(value.trim_end().to_owned())
}

fn confirm(question: &str) -> Result<bool> {
    let answer = prompt(&format!("{question} [y/N] "))?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn terminal_text(value: &str, limit: usize) -> String {
    let mut cleaned = String::new();
    let mut visible = 0;
    let mut pending_space = false;
    for character in value.chars() {
        if character.is_whitespace() || character.is_control() {
            pending_space = !cleaned.is_empty();
            continue;
        }
        if pending_space {
            if visible == limit {
                cleaned.push('…');
                break;
            }
            cleaned.push(' ');
            visible += 1;
            pending_space = false;
        }
        if visible == limit {
            cleaned.push('…');
            break;
        }
        cleaned.push(character);
        visible += 1;
    }
    if cleaned.is_empty() {
        "-".to_owned()
    } else {
        cleaned
    }
}

fn executable_exists(program: &OsStr) -> bool {
    let path = Path::new(program);
    if path.components().count() > 1 {
        return path.is_file();
    }
    env::var_os("PATH").is_some_and(|path_value| {
        env::split_paths(&path_value).any(|directory| directory.join(path).is_file())
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        SplitArg, SplitDirection, should_start_new, tab_title, terminal_text,
        validate_provider_session_id,
    };
    use crate::config::AgentKind;
    use crate::model::RuntimeState;

    #[test]
    fn split_names_match_tmux_geometry() {
        assert_eq!(
            SplitDirection::from(SplitArg::Horizontal),
            SplitDirection::LeftRight
        );
        assert_eq!(
            SplitDirection::from(SplitArg::Vertical),
            SplitDirection::TopBottom
        );
    }

    #[test]
    fn terminal_text_removes_controls_and_limits_length() {
        assert_eq!(terminal_text("a\n\x1bb", 20), "a b");
        assert_eq!(terminal_text("abcdef", 3), "abc…");
        assert_eq!(terminal_text("\n", 3), "-");
    }

    #[test]
    fn tab_titles_use_the_last_component() {
        assert_eq!(tab_title(Path::new("/work/project")), "project");
    }

    #[test]
    fn a_confirmed_provider_is_resumed_even_during_the_launch_race() {
        assert!(should_start_new(RuntimeState::Creating, false));
        assert!(!should_start_new(RuntimeState::Creating, true));
        assert!(!should_start_new(RuntimeState::Running, false));
    }

    #[test]
    fn resume_rejects_empty_session_ids_for_any_agent() {
        assert!(validate_provider_session_id(AgentKind::Claude, "").is_err());
        assert!(validate_provider_session_id(AgentKind::Claude, "   ").is_err());
        assert!(validate_provider_session_id(AgentKind::Codex, "").is_err());
    }

    #[test]
    fn resume_requires_a_uuid_for_claude_but_not_codex() {
        assert!(validate_provider_session_id(AgentKind::Claude, "not-a-uuid").is_err());
        assert!(
            validate_provider_session_id(AgentKind::Claude, "f9b8c7d6-1111-4222-8333-444455556666")
                .is_ok()
        );
        // Codex session IDs are opaque; WIPS doesn't know their format.
        assert!(validate_provider_session_id(AgentKind::Codex, "not-a-uuid").is_ok());
    }
}
