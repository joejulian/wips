use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SplitDirection {
    LeftRight,
    TopBottom,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CreatedPane {
    pub(crate) window_id: String,
    pub(crate) pane_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WindowSnapshot {
    pub(crate) window_id: String,
    pub(crate) tab_id: String,
    pub(crate) index: i64,
    pub(crate) title: String,
    pub(crate) layout: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PaneSnapshot {
    pub(crate) window_id: String,
    pub(crate) pane_id: String,
    pub(crate) session_id: String,
    pub(crate) index: i64,
    pub(crate) dead: bool,
    pub(crate) exit_code: Option<i32>,
}

#[derive(Clone, Debug)]
pub(crate) struct Tmux {
    socket: String,
    session: String,
    executable: PathBuf,
}

impl Tmux {
    pub(crate) fn new(
        socket: impl Into<String>,
        session: impl Into<String>,
        executable: PathBuf,
    ) -> Self {
        Self {
            socket: socket.into(),
            session: session.into(),
            executable,
        }
    }

    pub(crate) fn probe() -> Result<String> {
        let output = Command::new("tmux")
            .arg("-V")
            .output()
            .context("run tmux -V")?;
        if !output.status.success() {
            bail!("tmux -V failed: {}", stderr_text(&output));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    pub(crate) fn session_exists(&self) -> Result<bool> {
        let output = self.output([
            OsString::from("has-session"),
            OsString::from("-t"),
            self.session.clone().into(),
        ])?;
        Ok(output.status.success())
    }

    pub(crate) fn configure(&self) -> Result<()> {
        let commands: &[&[&str]] = &[
            &["set-option", "-g", "mouse", "on"],
            &["set-option", "-g", "renumber-windows", "on"],
            &["set-option", "-g", "status-left", "#[bold] WIPS #[default]"],
            &[
                "set-option",
                "-g",
                "status-right",
                "prefix+c new tab  |  prefix+%/\" split  |  prefix+x complete  |  prefix+f search",
            ],
            &["set-window-option", "-g", "remain-on-exit", "on"],
            &["set-window-option", "-g", "automatic-rename", "off"],
            &["set-window-option", "-g", "pane-border-status", "top"],
            &[
                "set-window-option",
                "-g",
                "pane-border-format",
                "#{?pane_active,#[bold],} #{pane_title} #[default]",
            ],
        ];
        for args in commands {
            self.run(args.iter().copied())?;
        }

        let executable = self.executable.as_os_str();
        self.run([
            OsStr::new("bind-key"),
            OsStr::new("c"),
            OsStr::new("new-window"),
            OsStr::new("-c"),
            OsStr::new("#{pane_current_path}"),
            executable,
            OsStr::new("_create"),
        ])?;
        self.run([
            OsStr::new("bind-key"),
            OsStr::new("%"),
            OsStr::new("split-window"),
            OsStr::new("-h"),
            OsStr::new("-c"),
            OsStr::new("#{pane_current_path}"),
            executable,
            OsStr::new("_create"),
        ])?;
        self.run([
            OsStr::new("bind-key"),
            OsStr::new("\""),
            OsStr::new("split-window"),
            OsStr::new("-v"),
            OsStr::new("-c"),
            OsStr::new("#{pane_current_path}"),
            executable,
            OsStr::new("_create"),
        ])?;
        self.bind_popup(
            "x",
            "Complete WIP",
            ["close", "--confirm", "--pane", "#{pane_id}"],
            "60",
            "7",
        )?;
        self.bind_popup(
            "&",
            "Complete tab",
            ["close", "--confirm", "--window", "#{window_id}"],
            "60",
            "7",
        )?;
        self.bind_popup(
            "r",
            "Resume WIP",
            ["resume", "--pane", "#{pane_id}"],
            "60",
            "7",
        )?;
        self.bind_popup("f", "Search WIPS", ["search"], "90%", "80%")?;

        Ok(())
    }

    pub(crate) fn create_session(
        &self,
        tab_id: &str,
        session_id: &str,
        title: &str,
        cwd: &Path,
    ) -> Result<CreatedPane> {
        let output = self.checked_output([
            OsString::from("new-session"),
            OsString::from("-d"),
            OsString::from("-P"),
            OsString::from("-F"),
            OsString::from("#{window_id}\t#{pane_id}"),
            OsString::from("-s"),
            self.session.clone().into(),
            OsString::from("-n"),
            title.into(),
            OsString::from("-c"),
            cwd.as_os_str().to_owned(),
            self.executable.as_os_str().to_owned(),
            OsString::from("_hold"),
        ])?;
        let pane = parse_created_pane(&output.stdout)?;
        self.tag_window(&pane.window_id, tab_id)?;
        self.tag_pane(&pane.pane_id, session_id, title)?;
        self.configure()?;
        self.respawn(&pane.pane_id, session_id, cwd)?;
        self.set_pane_title(&pane.pane_id, title)?;
        Ok(pane)
    }

    pub(crate) fn new_window(
        &self,
        tab_id: &str,
        session_id: &str,
        title: &str,
        cwd: &Path,
    ) -> Result<CreatedPane> {
        let output = self.checked_output([
            OsString::from("new-window"),
            OsString::from("-d"),
            OsString::from("-P"),
            OsString::from("-F"),
            OsString::from("#{window_id}\t#{pane_id}"),
            OsString::from("-t"),
            format!("{}:", self.session).into(),
            OsString::from("-n"),
            title.into(),
            OsString::from("-c"),
            cwd.as_os_str().to_owned(),
            self.executable.as_os_str().to_owned(),
            OsString::from("_run"),
            OsString::from("--session"),
            session_id.into(),
        ])?;
        let pane = parse_created_pane(&output.stdout)?;
        self.tag_window(&pane.window_id, tab_id)?;
        self.tag_pane(&pane.pane_id, session_id, title)?;
        Ok(pane)
    }

    pub(crate) fn split(
        &self,
        target: &str,
        direction: SplitDirection,
        session_id: &str,
        title: &str,
        cwd: &Path,
    ) -> Result<CreatedPane> {
        let direction_arg = match direction {
            SplitDirection::LeftRight => "-h",
            SplitDirection::TopBottom => "-v",
        };
        let output = self.checked_output([
            OsString::from("split-window"),
            direction_arg.into(),
            OsString::from("-d"),
            OsString::from("-P"),
            OsString::from("-F"),
            OsString::from("#{window_id}\t#{pane_id}"),
            OsString::from("-t"),
            target.into(),
            OsString::from("-c"),
            cwd.as_os_str().to_owned(),
            self.executable.as_os_str().to_owned(),
            OsString::from("_run"),
            OsString::from("--session"),
            session_id.into(),
        ])?;
        let pane = parse_created_pane(&output.stdout)?;
        self.tag_pane(&pane.pane_id, session_id, title)?;
        Ok(pane)
    }

    pub(crate) fn respawn(&self, pane_id: &str, session_id: &str, cwd: &Path) -> Result<()> {
        self.run([
            OsString::from("respawn-pane"),
            OsString::from("-k"),
            OsString::from("-t"),
            pane_id.into(),
            OsString::from("-c"),
            cwd.as_os_str().to_owned(),
            self.executable.as_os_str().to_owned(),
            OsString::from("_run"),
            OsString::from("--session"),
            session_id.into(),
        ])
    }

    pub(crate) fn apply_layout(&self, window_id: &str, layout: &str) -> Result<()> {
        if layout.is_empty() {
            return Ok(());
        }
        self.run(["select-layout", "-t", window_id, layout])
    }

    pub(crate) fn kill_pane(&self, pane_id: &str) -> Result<()> {
        self.run(["kill-pane", "-t", pane_id])
    }

    pub(crate) fn kill_window(&self, window_id: &str) -> Result<()> {
        self.run(["kill-window", "-t", window_id])
    }

    pub(crate) fn select_pane(&self, pane_id: &str) -> Result<()> {
        self.run(["select-pane", "-t", pane_id])
    }

    pub(crate) fn set_pane_title(&self, pane_id: &str, title: &str) -> Result<()> {
        self.run(["select-pane", "-t", pane_id, "-T", title])
    }

    pub(crate) fn set_window_title(&self, window_id: &str, title: &str) -> Result<()> {
        self.run(["rename-window", "-t", window_id, title])
    }

    pub(crate) fn tag_existing(
        &self,
        window_id: &str,
        tab_id: &str,
        pane_id: &str,
        session_id: &str,
        title: &str,
    ) -> Result<()> {
        self.tag_window(window_id, tab_id)?;
        self.tag_pane(pane_id, session_id, title)
    }

    pub(crate) fn snapshots(&self) -> Result<(Vec<WindowSnapshot>, Vec<PaneSnapshot>)> {
        let windows_output = self.checked_output([
            "list-windows",
            "-t",
            &self.session,
            "-F",
            "#{window_id}\t#{@wips_tab_id}\t#{window_index}\t#{window_name}\t#{window_layout}",
        ])?;
        let panes_output = self.checked_output([
            "list-panes",
            "-s",
            "-t",
            &self.session,
            "-F",
            "#{window_id}\t#{pane_id}\t#{@wips_session_id}\t#{pane_index}\t#{pane_dead}\t#{pane_dead_status}",
        ])?;

        Ok((
            parse_windows(&windows_output.stdout)?,
            parse_panes(&panes_output.stdout)?,
        ))
    }

    pub(crate) fn current_ids(&self, pane_id: &str) -> Result<(String, String)> {
        let output = self.checked_output([
            "display-message",
            "-p",
            "-t",
            pane_id,
            "#{window_id}\t#{@wips_tab_id}",
        ])?;
        let text =
            String::from_utf8(output.stdout).context("tmux returned non-UTF-8 identifiers")?;
        let (window_id, tab_id) = text
            .trim_end()
            .split_once('\t')
            .context("tmux did not return a window and tab identifier")?;
        Ok((window_id.to_owned(), tab_id.to_owned()))
    }

    pub(crate) fn attach(&self) -> Result<()> {
        let status = self
            .base_command()
            .args(["attach-session", "-t", &self.session])
            .env_remove("TMUX")
            .status()
            .context("attach to WIPS tmux session")?;
        if !status.success() {
            bail!("tmux attach-session exited with {status}");
        }
        Ok(())
    }

    fn bind_popup<const N: usize>(
        &self,
        key: &str,
        title: &str,
        args: [&str; N],
        width: &str,
        height: &str,
    ) -> Result<()> {
        let mut command = vec![
            OsString::from("bind-key"),
            key.into(),
            OsString::from("display-popup"),
            OsString::from("-E"),
            OsString::from("-w"),
            width.into(),
            OsString::from("-h"),
            height.into(),
            OsString::from("-T"),
            title.into(),
            self.executable.as_os_str().to_owned(),
        ];
        command.extend(args.into_iter().map(OsString::from));
        self.run(command)
    }

    fn tag_window(&self, window_id: &str, tab_id: &str) -> Result<()> {
        self.run(["set-option", "-w", "-t", window_id, "@wips_tab_id", tab_id])
    }

    fn tag_pane(&self, pane_id: &str, session_id: &str, title: &str) -> Result<()> {
        self.run([
            "set-option",
            "-p",
            "-t",
            pane_id,
            "@wips_session_id",
            session_id,
        ])?;
        self.set_pane_title(pane_id, title)
    }

    fn run<I, S>(&self, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.checked_output(args)?;
        debug_assert!(output.status.success());
        Ok(())
    }

    fn checked_output<I, S>(&self, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.output(args)?;
        if !output.status.success() {
            bail!("tmux command failed: {}", stderr_text(&output));
        }
        Ok(output)
    }

    fn output<I, S>(&self, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.base_command()
            .args(args)
            .stdin(Stdio::null())
            .output()
            .context("run tmux command")
    }

    fn base_command(&self) -> Command {
        let mut command = Command::new("tmux");
        command.args(["-L", &self.socket]);
        command
    }
}

fn parse_created_pane(bytes: &[u8]) -> Result<CreatedPane> {
    let text = std::str::from_utf8(bytes).context("tmux returned non-UTF-8 identifiers")?;
    let (window_id, pane_id) = text
        .trim_end()
        .split_once('\t')
        .context("tmux did not return a window and pane identifier")?;
    Ok(CreatedPane {
        window_id: window_id.to_owned(),
        pane_id: pane_id.to_owned(),
    })
}

fn parse_windows(bytes: &[u8]) -> Result<Vec<WindowSnapshot>> {
    let text = std::str::from_utf8(bytes).context("tmux returned non-UTF-8 window data")?;
    text.lines()
        .map(|line| {
            let mut fields = line.splitn(5, '\t');
            let window_id = next_field(&mut fields, "window id")?;
            let tab_id = next_field(&mut fields, "tab id")?;
            let index = next_field(&mut fields, "window index")?
                .parse()
                .context("parse tmux window index")?;
            let title = next_field(&mut fields, "window title")?;
            let layout = next_field(&mut fields, "window layout")?;
            Ok(WindowSnapshot {
                window_id,
                tab_id,
                index,
                title,
                layout,
            })
        })
        .collect()
}

fn parse_panes(bytes: &[u8]) -> Result<Vec<PaneSnapshot>> {
    let text = std::str::from_utf8(bytes).context("tmux returned non-UTF-8 pane data")?;
    text.lines()
        .map(|line| {
            let mut fields = line.splitn(6, '\t');
            let window_id = next_field(&mut fields, "window id")?;
            let pane_id = next_field(&mut fields, "pane id")?;
            let session_id = next_field(&mut fields, "WIPS session id")?;
            let index = next_field(&mut fields, "pane index")?
                .parse()
                .context("parse tmux pane index")?;
            let dead = match next_field(&mut fields, "pane state")?.as_str() {
                "0" => false,
                "1" => true,
                value => bail!("unknown tmux pane state {value:?}"),
            };
            let exit_code = match next_field(&mut fields, "pane exit status")?.as_str() {
                "" => None,
                value => Some(value.parse().context("parse tmux pane exit status")?),
            };
            Ok(PaneSnapshot {
                window_id,
                pane_id,
                session_id,
                index,
                dead,
                exit_code,
            })
        })
        .collect()
}

fn next_field<'a>(fields: &mut impl Iterator<Item = &'a str>, name: &str) -> Result<String> {
    fields
        .next()
        .map(str::to_owned)
        .with_context(|| format!("tmux output is missing {name}"))
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::{parse_created_pane, parse_panes, parse_windows};

    #[test]
    fn parses_created_pane() {
        let got = parse_created_pane(b"@3\t%7\n").expect("parse created pane");
        assert_eq!(got.window_id, "@3");
        assert_eq!(got.pane_id, "%7");
    }

    #[test]
    fn parses_snapshots() {
        let windows =
            parse_windows(b"@1\ttab-a\t2\twork\tabcd,80x24,0,0,1\n").expect("parse windows");
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].tab_id, "tab-a");
        assert_eq!(windows[0].index, 2);

        let panes = parse_panes(b"@1\t%2\tsession-a\t0\t0\t\n@1\t%3\tsession-b\t1\t1\t17\n")
            .expect("parse panes");
        assert_eq!(panes.len(), 2);
        assert!(!panes[0].dead);
        assert_eq!(panes[1].index, 1);
        assert!(panes[1].dead);
        assert_eq!(panes[1].exit_code, Some(17));
    }
}
