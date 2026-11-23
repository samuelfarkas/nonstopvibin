//! Start at login: the `~/.config/autostart` entry on Linux (`desktop/autostart.ts`), the
//! `SMAppService.mainApp` registration Electron made on macOS.

#[cfg(target_os = "linux")]
pub use linux::Login;
#[cfg(target_os = "macos")]
pub use macos::Login;

/// A development run refuses, as main.ts did.
pub(crate) const UNPACKAGED: &str = "Use the packaged app to enable automatic startup.";

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::OsString;
    use std::fs::{self, OpenOptions};
    use std::io::{self, Write};
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};

    const FILE: &str = "app.nonstopvibin.desktop";

    /// The autostart entry and the executable it starts.
    #[derive(Debug, Clone)]
    pub struct Login {
        autostart: PathBuf,
        executable: PathBuf,
    }

    impl Login {
        /// `config` is Electron's `appData`: `$XDG_CONFIG_HOME`, else `~/.config`.
        #[must_use]
        pub fn new(config: &Path, executable: PathBuf) -> Self {
            Self {
                autostart: config.join("autostart"),
                executable,
            }
        }

        /// From the environment: the configuration folder, and `$APPIMAGE` (the mounted image
        /// moves with every launch) or else this executable.
        ///
        /// # Errors
        /// When neither `$XDG_CONFIG_HOME` nor `$HOME` is set, or the executable's path is unknown.
        pub fn from_environment() -> io::Result<Self> {
            let absolute = |value: Option<OsString>| {
                value.map(PathBuf::from).filter(|path| path.is_absolute())
            };
            let config = absolute(std::env::var_os("XDG_CONFIG_HOME"))
                .or_else(|| absolute(std::env::var_os("HOME")).map(|home| home.join(".config")))
                .ok_or_else(|| io::Error::other("neither XDG_CONFIG_HOME nor HOME is set"))?;
            let executable = match absolute(std::env::var_os("APPIMAGE")) {
                Some(image) => image,
                None => std::env::current_exe()?,
            };
            Ok(Self::new(&config, executable))
        }

        fn file(&self) -> PathBuf {
            self.autostart.join(FILE)
        }

        pub(crate) fn enabled(&self) -> bool {
            self.file().exists()
        }

        pub(crate) fn set(&self, enabled: bool) -> Result<bool, String> {
            let file = self.file();
            if enabled {
                let entry = desktop_entry(&self.executable)?;
                if fs::read(&file).is_ok_and(|current| current == entry.as_bytes()) {
                    return Ok(true);
                }
                fs::create_dir_all(&self.autostart).map_err(|error| error.to_string())?;
                // As Node's writeFile: the mode applies to a new file only.
                OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&file)
                    .and_then(|mut opened| opened.write_all(entry.as_bytes()))
                    .map_err(|error| error.to_string())?;
            } else {
                match fs::remove_file(&file) {
                    Err(error) if error.kind() != io::ErrorKind::NotFound => {
                        return Err(error.to_string());
                    }
                    _ => {}
                }
            }
            Ok(self.enabled())
        }

        /// An update may rename the `AppImage`: points an existing entry at this executable and
        /// creates none.
        pub(crate) fn follow(&self) -> Result<(), String> {
            if self.enabled() {
                self.set(true)?;
            }
            Ok(())
        }
    }

    /// `desktopEntry` in autostart.ts, byte for byte.
    pub(super) fn desktop_entry(executable: &Path) -> Result<String, String> {
        let Some(executable) = executable.to_str() else {
            return Err(
                "Move the app to a path that is valid UTF-8 to enable automatic startup."
                    .to_owned(),
            );
        };
        if executable.contains(['\n', '\r', '=']) {
            return Err("Move the app to a path without newlines or equals signs to enable automatic startup.".to_owned());
        }
        // Desktop Entry string escaping is applied before Exec argument escaping.
        let mut quoted = String::with_capacity(executable.len());
        for character in executable.chars() {
            if matches!(character, '\\' | '"' | '`' | '$') {
                quoted.push('\\');
            }
            quoted.push(character);
        }
        let argument = quoted.replace('\\', "\\\\").replace('%', "%%");
        Ok(format!(
            "[Desktop Entry]\nType=Application\nName=NonstopVibin\nExec=\"{argument}\" --background\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
        ))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn entry_exec(executable: &str) -> String {
            let entry = desktop_entry(Path::new(executable)).unwrap();
            entry
                .lines()
                .find_map(|line| line.strip_prefix("Exec="))
                .unwrap()
                .to_owned()
        }

        #[test]
        fn the_entry_is_autostart_ts_entry() {
            assert_eq!(
                desktop_entry(Path::new("/opt/NonstopVibin/nonstopvibin")).unwrap(),
                "[Desktop Entry]\nType=Application\nName=NonstopVibin\nExec=\"/opt/NonstopVibin/nonstopvibin\" --background\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
            );
            // Worked through autostart.ts's two passes by hand.
            assert_eq!(
                entry_exec(r#"/home/a b/50%/"q"/`x`/$HOME/back\slash"#),
                r#""/home/a b/50%%/\\"q\\"/\\`x\\`/\\$HOME/back\\\\slash" --background"#
            );
        }

        #[test]
        fn refuses_paths_a_desktop_entry_cannot_hold() {
            for path in ["/a\nb", "/a\rb", "/a=b"] {
                assert_eq!(
                    desktop_entry(Path::new(path)),
                    Err("Move the app to a path without newlines or equals signs to enable automatic startup.".to_owned())
                );
            }
        }

        fn temporary(name: &str) -> PathBuf {
            let path =
                std::env::temp_dir().join(format!("nsv-shell-login-{name}-{}", std::process::id()));
            drop(fs::remove_dir_all(&path));
            path
        }

        #[test]
        fn enabling_writes_an_owner_only_entry_and_disabling_removes_it() {
            let config = temporary("toggle");
            let login = Login::new(&config, PathBuf::from("/opt/nv/nonstopvibin"));
            assert!(!login.enabled());
            assert_eq!(login.set(true), Ok(true));
            let file = config.join("autostart/app.nonstopvibin.desktop");
            assert_eq!(
                fs::read_to_string(&file).unwrap(),
                desktop_entry(Path::new("/opt/nv/nonstopvibin")).unwrap()
            );
            assert_eq!(
                fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(login.set(false), Ok(false));
            assert!(!file.exists());
            assert_eq!(login.set(false), Ok(false));
            fs::remove_dir_all(config).unwrap();
        }

        #[test]
        fn following_a_renamed_appimage_rewrites_only_an_existing_entry() {
            let config = temporary("follow");
            let renamed = Login::new(&config, PathBuf::from("/apps/NonstopVibin-0.3.0.AppImage"));
            renamed.follow().unwrap();
            assert!(!renamed.enabled(), "following must not opt in");
            Login::new(&config, PathBuf::from("/apps/NonstopVibin-0.2.0.AppImage"))
                .set(true)
                .unwrap();
            renamed.follow().unwrap();
            let entry =
                fs::read_to_string(config.join("autostart/app.nonstopvibin.desktop")).unwrap();
            assert!(entry.contains("Exec=\"/apps/NonstopVibin-0.3.0.AppImage\" --background"));
            assert!(!entry.contains("0.2.0"), "{entry}");
            renamed.set(false).unwrap();
            assert!(!renamed.enabled());
            fs::remove_dir_all(config).unwrap();
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2_service_management::{SMAppService, SMAppServiceStatus};

    use crate::log;

    /// `SMAppService.mainApp`: the app bundle itself. Electron registered the same service
    /// (`setLoginItemSettings` on macOS 13 and later), so existing registrations carry over.
    #[derive(Debug, Clone, Default)]
    pub struct Login;

    impl Login {
        #[must_use]
        pub fn new() -> Self {
            Self
        }

        #[expect(unsafe_code, reason = "objc2 marks every SMAppService method unsafe")]
        #[expect(clippy::unused_self, reason = "the Linux twin reads its own entry")]
        pub(crate) fn enabled(&self) -> bool {
            // SAFETY: a class method without arguments that returns a retained object.
            let service = unsafe { SMAppService::mainAppService() };
            // SAFETY: a getter on that live object.
            let status = unsafe { service.status() };
            status == SMAppServiceStatus::Enabled
        }

        /// As `setLoginItemSettings` then `getLoginItemSettings`: macOS may refuse or ask the user,
        /// and only the status says what it did.
        #[expect(unsafe_code, reason = "objc2 marks every SMAppService method unsafe")]
        pub(crate) fn set(&self, enabled: bool) -> Result<bool, String> {
            // SAFETY: a class method without arguments that returns a retained object.
            let service = unsafe { SMAppService::mainAppService() };
            let changed = if enabled {
                // SAFETY: registers the live service object; errors come back as NSError.
                unsafe { service.registerAndReturnError() }
            } else {
                // SAFETY: unregisters the live service object; errors come back as NSError.
                unsafe { service.unregisterAndReturnError() }
            };
            if let Err(error) = changed {
                log(format_args!("the login item did not change: {error:?}"));
            }
            if self.enabled() == enabled {
                Ok(enabled)
            } else {
                Err("macOS did not apply this setting. Add NonstopVibin in System Settings → General → Login Items.".to_owned())
            }
        }

        #[expect(clippy::unnecessary_wraps, reason = "the Linux twin rewrites a file")]
        #[expect(clippy::unused_self, reason = "the Linux twin rewrites a file")]
        pub(crate) fn follow(&self) -> Result<(), String> {
            Ok(())
        }
    }
}
