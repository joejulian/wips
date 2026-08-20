use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use thiserror::Error;

const APPLICATION_NAME: &str = "wips";
const CONFIG_FILE_NAME: &str = "config.toml";
const DATABASE_FILE_NAME: &str = "wips.sqlite3";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Paths {
    pub(crate) config: PathBuf,
    pub(crate) state_dir: PathBuf,
    pub(crate) database: PathBuf,
}

#[derive(Debug, Error)]
pub(crate) enum PathError {
    #[error("could not determine the platform configuration directories")]
    PlatformDirectoriesUnavailable,

    #[error("{0} is set but empty")]
    EmptyOverride(&'static str),
}

impl Paths {
    pub(crate) fn discover() -> Result<Self> {
        let project_dirs = ProjectDirs::from("org", "Wips", APPLICATION_NAME)
            .ok_or(PathError::PlatformDirectoriesUnavailable)?;
        let default_state_dir = project_dirs
            .state_dir()
            .unwrap_or_else(|| project_dirs.data_local_dir());
        let cwd = env::current_dir().context("resolve the current working directory")?;

        Ok(Self::from_parts(
            project_dirs.config_dir(),
            default_state_dir,
            &cwd,
            env_override("WIPS_CONFIG")?,
            env_override("WIPS_STATE_DIR")?,
        ))
    }

    pub(crate) fn new(config: PathBuf, state_dir: PathBuf) -> Self {
        let database = state_dir.join(DATABASE_FILE_NAME);
        Self {
            config,
            state_dir,
            database,
        }
    }

    pub(crate) fn ensure_dirs(&self) -> Result<()> {
        if let Some(config_dir) = non_empty_parent(&self.config) {
            create_private_dir(config_dir).with_context(|| {
                format!(
                    "could not create configuration directory {}",
                    config_dir.display()
                )
            })?;
        }

        create_private_dir(&self.state_dir).with_context(|| {
            format!(
                "could not create state directory {}",
                self.state_dir.display()
            )
        })
    }

    fn from_parts(
        default_config_dir: &Path,
        default_state_dir: &Path,
        cwd: &Path,
        config_override: Option<PathBuf>,
        state_override: Option<PathBuf>,
    ) -> Self {
        let config = config_override.map_or_else(
            || default_config_dir.join(CONFIG_FILE_NAME),
            |path| absolute_from(cwd, path),
        );
        let state_dir = state_override.map_or_else(
            || default_state_dir.to_path_buf(),
            |path| absolute_from(cwd, path),
        );
        Self::new(config, state_dir)
    }
}

fn absolute_from(cwd: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

pub(crate) fn secure_created_file(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let metadata = fs::metadata(path)?;
        if metadata.mode() & 0o777 != 0o600 {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
    }

    #[cfg(not(unix))]
    let _ = path;

    Ok(())
}

fn env_override(variable: &'static str) -> std::result::Result<Option<PathBuf>, PathError> {
    parse_override(variable, env::var_os(variable))
}

fn parse_override(
    variable: &'static str,
    value: Option<OsString>,
) -> std::result::Result<Option<PathBuf>, PathError> {
    value
        .map(|value| {
            if value.is_empty() {
                Err(PathError::EmptyOverride(variable))
            } else {
                Ok(PathBuf::from(value))
            }
        })
        .transpose()
}

fn non_empty_parent(path: &Path) -> Option<&Path> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

        let existed = path.try_exists()?;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(path)?;
        if !existed {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
    }

    #[cfg(not(unix))]
    fs::create_dir_all(path)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn defaults_follow_platform_base_directories() {
        let paths = Paths::from_parts(
            Path::new("/config-home/wips"),
            Path::new("/state-home/wips"),
            Path::new("/work"),
            None,
            None,
        );

        assert_eq!(paths.config, Path::new("/config-home/wips/config.toml"));
        assert_eq!(paths.state_dir, Path::new("/state-home/wips"));
        assert_eq!(paths.database, Path::new("/state-home/wips/wips.sqlite3"));
    }

    #[test]
    fn explicit_paths_override_platform_directories() {
        let paths = Paths::from_parts(
            Path::new("/ignored/config"),
            Path::new("/ignored/state"),
            Path::new("/work"),
            Some(PathBuf::from("relative/config.toml")),
            Some(PathBuf::from("relative/state")),
        );

        assert_eq!(paths.config, Path::new("/work/relative/config.toml"));
        assert_eq!(paths.state_dir, Path::new("/work/relative/state"));
        assert_eq!(
            paths.database,
            Path::new("/work/relative/state/wips.sqlite3")
        );
    }

    #[test]
    fn creates_configuration_and_state_directories() {
        let temp = tempdir().expect("temporary directory should be created");
        let paths = Paths::new(
            temp.path().join("config/wips/config.toml"),
            temp.path().join("state/wips"),
        );

        paths.ensure_dirs().expect("directories should be created");

        assert!(paths.config.parent().expect("config parent").is_dir());
        assert!(paths.state_dir.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn created_directories_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempdir().expect("temporary directory should be created");
        let paths = Paths::new(
            temp.path().join("config/wips/config.toml"),
            temp.path().join("state/wips"),
        );

        paths.ensure_dirs().expect("directories should be created");

        let config_mode = fs::metadata(paths.config.parent().expect("config parent"))
            .expect("config metadata")
            .permissions()
            .mode()
            & 0o777;
        let state_mode = fs::metadata(&paths.state_dir)
            .expect("state metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(config_mode, 0o700);
        assert_eq!(state_mode, 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn marks_a_created_sensitive_file_private() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempdir().expect("temporary directory should be created");
        let path = temp.path().join("sensitive");
        fs::write(&path, "secret").expect("file should be written");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))
            .expect("test mode should be applied");

        secure_created_file(&path).expect("file should be secured");

        let mode = fs::metadata(path)
            .expect("file metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn bare_config_filename_does_not_treat_current_directory_as_app_owned() {
        assert!(non_empty_parent(Path::new("config.toml")).is_none());
    }

    #[test]
    fn empty_environment_override_is_rejected() {
        assert!(matches!(
            parse_override("WIPS_CONFIG", Some(OsString::new())),
            Err(PathError::EmptyOverride("WIPS_CONFIG"))
        ));
    }
}
