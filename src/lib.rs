//! A build dependency for Cargo libraries to find system artifacts through the
//! `pkg-config` utility.
//!
//! This library will shell out to `pkg-config` as part of build scripts and
//! probe the system to determine how to link to a specified library. The
//! `Config` structure serves as a method of configuring how `pkg-config` is
//! invoked in a builder style.
//!
//! A number of environment variables are available to globally configure how
//! this crate will invoke `pkg-config`:
//!
//! * `PKG_CONFIG_ALLOW_CROSS` - if this variable is not set, then `pkg-config`
//!   will automatically be disabled for all cross compiles.
//! * `FOO_NO_PKG_CONFIG` - if set, this will disable running `pkg-config` when
//!   probing for the library named `foo`.
//!
//! There are also a number of environment variables which can configure how a
//! library is linked to (dynamically vs statically). These variables control
//! whether the `--static` flag is passed. Note that this behavior can be
//! overridden by configuring explicitly on `Config`. The variables are checked
//! in the following order:
//!
//! * `FOO_STATIC` - pass `--static` for the library `foo`
//! * `FOO_DYNAMIC` - do not pass `--static` for the library `foo`
//! * `PKG_CONFIG_ALL_STATIC` - pass `--static` for all libraries
//! * `PKG_CONFIG_ALL_DYNAMIC` - do not pass `--static` for all libraries
//!
//! After running `pkg-config` all appropriate Cargo metadata will be printed on
//! stdout if the search was successful.
//!
//! # Example
//!
//! Find the system library named `foo`, with minimum version 1.2.3:
//!
//! ```no_run
//! extern crate pkg_config;
//!
//! fn main() {
//!     pkg_config::Config::new().atleast_version("1.2.3").probe("foo").unwrap();
//! }
//! ```
//!
//! Find the system library named `foo`, with no version requirement (not
//! recommended):
//!
//! ```no_run
//! extern crate pkg_config;
//!
//! fn main() {
//!     pkg_config::probe_library("foo").unwrap();
//! }
//! ```
//!
//! Configure how library `foo` is linked to.
//!
//! ```no_run
//! extern crate pkg_config;
//!
//! fn main() {
//!     pkg_config::Config::new().atleast_version("1.2.3").statik(true).probe("foo").unwrap();
//! }
//! ```

#![doc(html_root_url = "https://docs.rs/pkg-config/0.3")]

#[allow(unused_imports)] // Required for Rust <1.23
use std::ascii::AsciiExt;
use std::collections::HashMap;
use std::env;
use std::error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::{PathBuf, Path};
use std::process::{Command, Output};
use std::str;

pub fn target_supported() -> bool {
    let target = env::var("TARGET").unwrap_or_else(|_| String::new());
    let host = env::var("HOST").unwrap_or_else(|_| String::new());

    // Only use pkg-config in host == target situations by default (allowing an
    // override).
    (host == target || env::var_os("PKG_CONFIG_ALLOW_CROSS").is_some())
}

#[derive(Clone, PartialEq)]
pub enum Statik {
    No,
    Yes,
    Force,
}

impl From<bool> for Statik {
    fn from(o: bool) -> Self {
        match o {
            true => Statik::Yes,
            false => Statik::No,
        }
    }
}

#[derive(Clone, Default)]
pub struct Config {
    statik: Option<Statik>,
    statik_blacklist: Vec<String>,
    atleast_version: Option<String>,
    extra_args: Vec<OsString>,
    cargo_metadata: bool,
    env_metadata: bool,
    print_system_libs: bool,
}

#[derive(Debug)]
pub struct Library {
    pub libs: Vec<String>,
    pub link_paths: Vec<PathBuf>,
    pub frameworks: Vec<String>,
    pub framework_paths: Vec<PathBuf>,
    pub include_paths: Vec<PathBuf>,
    pub defines: HashMap<String, Option<String>>,
    pub version: String,
    _priv: (),
}

/// Represents all reasons `pkg-config` might not succeed or be run at all.
pub enum Error {
    /// Aborted because of `*_NO_PKG_CONFIG` environment variable.
    ///
    /// Contains the name of the responsible environment variable.
    EnvNoPkgConfig(String),

    /// Cross compilation detected.
    ///
    /// Override with `PKG_CONFIG_ALLOW_CROSS=1`.
    CrossCompilation,

    /// Failed to run `pkg-config`.
    ///
    /// Contains the command and the cause.
    Command { command: String, cause: io::Error },

    /// `pkg-config` did not exit sucessfully.
    ///
    /// Contains the command and output.
    Failure { command: String, output: Output },

    #[doc(hidden)]
    // please don't match on this, we're likely to add more variants over time
    __Nonexhaustive,
}

impl error::Error for Error {
    fn description(&self) -> &str {
        match *self {
            Error::EnvNoPkgConfig(_) => "pkg-config requested to be aborted",
            Error::CrossCompilation => {
                "pkg-config doesn't handle cross compilation. \
                 Use PKG_CONFIG_ALLOW_CROSS=1 to override"
            }
            Error::Command { .. } => "failed to run pkg-config",
            Error::Failure { .. } => "pkg-config did not exit sucessfully",
            Error::__Nonexhaustive => panic!(),
        }
    }

    fn cause(&self) -> Option<&error::Error> {
        match *self {
            Error::Command { ref cause, .. } => Some(cause),
            _ => None,
        }
    }
}

// Workaround for temporary lack of impl Debug for Output in stable std
struct OutputDebugger<'a>(&'a Output);

// Lifted from 1.7 std
impl<'a> fmt::Debug for OutputDebugger<'a> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let stdout_utf8 = str::from_utf8(&self.0.stdout);
        let stdout_debug: &fmt::Debug = match stdout_utf8 {
            Ok(ref str) => str,
            Err(_) => &self.0.stdout
        };

        let stderr_utf8 = str::from_utf8(&self.0.stderr);
        let stderr_debug: &fmt::Debug = match stderr_utf8 {
            Ok(ref str) => str,
            Err(_) => &self.0.stderr
        };

        fmt.debug_struct("Output")
           .field("status", &self.0.status)
           .field("stdout", stdout_debug)
           .field("stderr", stderr_debug)
           .finish()
    }
}

// Workaround for temporary lack of impl Debug for Output in stable std, continued
impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match *self {
            Error::EnvNoPkgConfig(ref name) => {
                f.debug_tuple("EnvNoPkgConfig")
                 .field(name)
                 .finish()
            }
            Error::CrossCompilation => write!(f, "CrossCompilation"),
            Error::Command { ref command, ref cause } => {
                f.debug_struct("Command")
                 .field("command", command)
                 .field("cause", cause)
                 .finish()
            }
            Error::Failure { ref command, ref output } => {
                f.debug_struct("Failure")
                 .field("command", command)
                 .field("output", &OutputDebugger(output))
                 .finish()
            }
            Error::__Nonexhaustive => panic!(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match *self {
            Error::EnvNoPkgConfig(ref name) => {
                write!(f, "Aborted because {} is set", name)
            }
            Error::CrossCompilation => {
                write!(f, "Cross compilation detected. \
                       Use PKG_CONFIG_ALLOW_CROSS=1 to override")
            }
            Error::Command { ref command, ref cause } => {
                write!(f, "Failed to run `{}`: {}", command, cause)
            }
            Error::Failure { ref command, ref output } => {
                let stdout = str::from_utf8(&output.stdout).unwrap();
                let stderr = str::from_utf8(&output.stderr).unwrap();
                write!(f, "`{}` did not exit successfully: {}", command, output.status)?;
                if !stdout.is_empty() {
                    write!(f, "\n--- stdout\n{}", stdout)?;
                }
                if !stderr.is_empty() {
                    write!(f, "\n--- stderr\n{}", stderr)?;
                }
                Ok(())
            }
            Error::__Nonexhaustive => panic!(),
        }
    }
}

/// Deprecated in favor of the probe_library function
#[doc(hidden)]
pub fn find_library(name: &str) -> Result<Library, String> {
    probe_library(name).map_err(|e| e.to_string())
}

/// Simple shortcut for using all default options for finding a library.
pub fn probe_library(name: &str) -> Result<Library, Error> {
    Config::new().probe(name)
}

/// Run `pkg-config` to get the value of a variable from a package using
/// --variable.
pub fn get_variable(package: &str, variable: &str) -> Result<String, Error> {
    let arg = format!("--variable={}", variable);
    let cfg = Config::new();
    let out = run(cfg.command(package, &[&arg]))?;
    Ok(str::from_utf8(&out).unwrap().trim_right().to_owned())
}

impl Config {
    /// Creates a new set of configuration options which are all initially set
    /// to "blank".
    pub fn new() -> Config {
        Config {
            statik: None,
            statik_blacklist: vec![],
            atleast_version: None,
            extra_args: vec![],
            print_system_libs: true,
            cargo_metadata: true,
            env_metadata: false,
        }
    }

    /// Indicate whether the `--static` flag should be passed.
    ///
    /// This will override the inference from environment variables described in
    /// the crate documentation.
    pub fn statik<S>(&mut self, statik: S) -> &mut Config
        where S: Into<Statik>
    {
        self.statik = Some(statik.into());
        self
    }

    pub fn statik_blacklist<S>(&mut self, blacklist: Vec<S>) -> &mut Config
        where String: From<S>, S: Clone
    {
        self.statik_blacklist.extend(
            blacklist
                .iter()
                .map(|s| String::from(s.clone()))
        );
        self
    }

    pub fn statik_blacklist_contains<S>(&self, val: S) -> bool
        where String: PartialEq<S>
    {
        self.statik_blacklist.iter().any(|s| s == &val)
    }

    /// Indicate that the library must be at least version `vers`.
    pub fn atleast_version(&mut self, vers: &str) -> &mut Config {
        self.atleast_version = Some(vers.to_string());
        self
    }

    /// Add an argument to pass to pkg-config.
    ///
    /// It's placed after all of the arguments generated by this library.
    pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Config {
        self.extra_args.push(arg.as_ref().to_os_string());
        self
    }

    /// Define whether metadata should be emitted for cargo allowing it to
    /// automatically link the binary. Defaults to `true`.
    pub fn cargo_metadata(&mut self, cargo_metadata: bool) -> &mut Config {
        self.cargo_metadata = cargo_metadata;
        self
    }

    /// Define whether metadata should be emitted for cargo allowing to
    /// automatically rebuild when environment variables change. Defaults to
    /// `false`.
    pub fn env_metadata(&mut self, env_metadata: bool) -> &mut Config {
        self.env_metadata = env_metadata;
        self
    }

    /// Enable or disable the `PKG_CONFIG_ALLOW_SYSTEM_LIBS` environment
    /// variable.
    ///
    /// This env var is enabled by default.
    pub fn print_system_libs(&mut self, print: bool) -> &mut Config {
        self.print_system_libs = print;
        self
    }

    /// Deprecated in favor fo the `probe` function
    #[doc(hidden)]
    pub fn find(&self, name: &str) -> Result<Library, String> {
        self.probe(name).map_err(|e| e.to_string())
    }

    /// Run `pkg-config` to find the library `name`.
    ///
    /// This will use all configuration previously set to specify how
    /// `pkg-config` is run.
    pub fn probe(&self, name: &str) -> Result<Library, Error> {
        let abort_var_name = format!("{}_NO_PKG_CONFIG", envify(name));
        if self.env_var_os(&abort_var_name).is_some() {
            return Err(Error::EnvNoPkgConfig(abort_var_name))
        } else if !target_supported() {
            return Err(Error::CrossCompilation);
        }

        let mut library = Library::new();

        let output = run(self.command(name, &["--libs", "--cflags"]))?;
        library.parse_libs_cflags(name, &output, self);

        let output = run(self.command(name, &["--modversion"]))?;
        library.parse_modversion(str::from_utf8(&output).unwrap());

        Ok(library)
    }

    /// Deprecated in favor of the top level `get_variable` function
    #[doc(hidden)]
    pub fn get_variable(package: &str, variable: &str) -> Result<String, String> {
        get_variable(package, variable).map_err(|e| e.to_string())
    }

    fn targetted_env_var(&self, var_base: &str) -> Result<String, env::VarError> {
        if let Ok(target) = env::var("TARGET") {
            let host = env::var("HOST")?;
            let kind = if host == target { "HOST" } else { "TARGET" };
            let target_u = target.replace("-", "_");

            self.env_var(&format!("{}_{}", var_base, target))
                .or_else(|_| self.env_var(&format!("{}_{}", var_base, target_u)))
                .or_else(|_| self.env_var(&format!("{}_{}", kind, var_base)))
                .or_else(|_| self.env_var(var_base))
        } else {
            self.env_var(var_base)
        }
    }

    fn env_var(&self, name: &str) -> Result<String, env::VarError> {
        if self.env_metadata {
            println!("cargo:rerun-if-env-changed={}", name);
        }
        env::var(name)
    }

    fn env_var_os(&self, name: &str) -> Option<OsString> {
        if self.env_metadata {
            println!("cargo:rerun-if-env-changed={}", name);
        }
        env::var_os(name)
    }

    fn is_static(&self, name: &str) -> Statik {
        if self.statik_blacklist_contains(name) {
            Statik::No
        } else {
            match self.statik {
                Some(ref statik) => statik.clone(),
                None => self.infer_static(name),
            }
        }
    }

    fn command(&self, name: &str, args: &[&str]) -> Command {
        let exe = self.env_var("PKG_CONFIG").unwrap_or_else(|_| String::from("pkg-config"));
        let mut cmd = Command::new(exe);
        if self.is_static(name) != Statik::No {
            cmd.arg("--static");
        }
        cmd.args(args)
           .args(&self.extra_args);

        if let Ok(value) = self.targetted_env_var("PKG_CONFIG_PATH") {
            cmd.env("PKG_CONFIG_PATH", value);
        }
        if let Ok(value) = self.targetted_env_var("PKG_CONFIG_LIBDIR") {
            cmd.env("PKG_CONFIG_LIBDIR", value);
        }
        if let Ok(value) = self.targetted_env_var("PKG_CONFIG_SYSROOT_DIR") {
            cmd.env("PKG_CONFIG_SYSROOT_DIR", value);
        }
        if self.print_system_libs {
            cmd.env("PKG_CONFIG_ALLOW_SYSTEM_LIBS", "1");
        }
        if let Some(ref version) = self.atleast_version {
            cmd.arg(&format!("{} >= {}", name, version));
        } else {
            cmd.arg(name);
        }
        cmd
    }

    fn print_metadata(&self, s: &str) {
        if self.cargo_metadata {
            println!("cargo:{}", s);
        }
    }

    fn infer_static(&self, name: &str) -> Statik {
        let name = envify(name);
        if self.env_var_os(&format!("{}_STATIC_FORCE", name)).is_some() {
            Statik::Force
        } else if self.env_var_os(&format!("{}_STATIC", name)).is_some() {
            Statik::Yes
        } else if self.env_var_os(&format!("{}_DYNAMIC", name)).is_some() {
            Statik::No
        } else if self.env_var_os("PKG_CONFIG_ALL_STATIC").is_some() {
            Statik::Yes
        } else if self.env_var_os("PKG_CONFIG_ALL_DYNAMIC").is_some() {
            Statik::No
        } else {
            Statik::No
        }
    }
}

impl Library {
    fn new() -> Library {
        Library {
            libs: Vec::new(),
            link_paths: Vec::new(),
            include_paths: Vec::new(),
            frameworks: Vec::new(),
            framework_paths: Vec::new(),
            defines: HashMap::new(),
            version: String::new(),
            _priv: (),
        }
    }

    fn parse_libs_cflags(&mut self, name: &str, output: &[u8], config: &Config) {
        let mut is_msvc = false;
        if let Ok(target) = env::var("TARGET") {
            if target.contains("msvc") {
                is_msvc = true;
            }
        }

        let words = split_flags(output);
        let parts = words.iter()
                          .filter(|l| l.len() > 2)
                          .map(|arg| (&arg[0..2], &arg[2..]))
                          .collect::<Vec<_>>();

        let mut dirs = Vec::new();
        let statik = config.is_static(name);
        for &(flag, val) in &parts {
            match flag {
                "-L" => {
                    let meta = format!("rustc-link-search=native={}", val);
                    config.print_metadata(&meta);
                    dirs.push(PathBuf::from(val));
                    self.link_paths.push(PathBuf::from(val));
                }
                "-F" => {
                    let meta = format!("rustc-link-search=framework={}", val);
                    config.print_metadata(&meta);
                    self.framework_paths.push(PathBuf::from(val));
                }
                "-I" => {
                    self.include_paths.push(PathBuf::from(val));
                }
                "-l" => {
                    // These are provided by the CRT with MSVC
                    if is_msvc && ["m", "c", "pthread"].contains(&val) {
                        continue;
                    }

                    if match statik {
                        Statik::Force => true,
                        Statik::Yes => is_static_available(val, &dirs),
                        Statik::No => false,
                    } && !config.statik_blacklist_contains(val) {
                        let meta = format!("rustc-link-lib=static={}", val);
                        config.print_metadata(&meta);
                    } else {
                        let meta = format!("rustc-link-lib={}", val);
                        config.print_metadata(&meta);
                    }

                    self.libs.push(val.to_string());
                }
                "-D" => {
                    let mut iter = val.split("=");
                    self.defines.insert(iter.next().unwrap().to_owned(), iter.next().map(|s| s.to_owned()));
                }
                _ => {}
            }
        }

        let mut iter = words.iter()
                            .flat_map(|arg| if arg.starts_with("-Wl,") {
                                 arg[4..].split(',').collect()
                             } else {
                                 vec![arg.as_ref()]
                             });
        while let Some(part) = iter.next() {
            if part != "-framework" {
                continue
            }
            if let Some(lib) = iter.next() {
                let meta = format!("rustc-link-lib=framework={}", lib);
                config.print_metadata(&meta);
                self.frameworks.push(lib.to_string());
            }
        }
    }

    fn parse_modversion(&mut self, output: &str) {
        self.version.push_str(output.trim());
    }
}

fn envify(name: &str) -> String {
    name.chars().map(|c| c.to_ascii_uppercase()).map(|c| {
        if c == '-' {'_'} else {c}
    }).collect()
}

/// System libraries should only be linked dynamically
fn is_static_available(name: &str, dirs: &[PathBuf]) -> bool {
    let libname = format!("lib{}.a", name);
    let system_roots = if cfg!(target_os = "macos") {
        vec![Path::new("/Library"), Path::new("/System")]
    } else {
        vec![Path::new("/usr")]
    };

    dirs.iter().any(|dir| {
        !system_roots.iter().any(|sys| dir.starts_with(sys)) &&
        dir.join(&libname).exists()
    })
}

fn run(mut cmd: Command) -> Result<Vec<u8>, Error> {
    match cmd.output() {
        Ok(output) => {
            if output.status.success() {
                Ok(output.stdout)
            } else {
                Err(Error::Failure {
                    command: format!("{:?}", cmd),
                    output: output,
                })
            }
        }
        Err(cause) => Err(Error::Command {
            command: format!("{:?}", cmd),
            cause: cause,
        }),
    }
}

/// Split output produced by pkg-config --cflags and / or --libs into separate flags.
///
/// Backslash in output is used to preserve literal meaning of following byte.  Different words are
/// separated by unescaped space. Other whitespace characters generally should not occur unescaped
/// at all, apart from the newline at the end of output. For compatibility with what others
/// consumers of pkg-config output would do in this scenario, they are used here for splitting as
/// well.
fn split_flags(output: &[u8]) -> Vec<String> {
    let mut word = Vec::new();
    let mut words = Vec::new();
    let mut escaped = false;

    for &b in output {
        match b {
            _ if escaped => {
                escaped = false;
                word.push(b);
            }
            b'\\' => {
                escaped = true
            }
            b'\t' | b'\n' | b'\r' | b' ' => {
                if !word.is_empty() {
                    words.push(String::from_utf8(word).unwrap());
                    word = Vec::new();
                }
            }
            _ => word.push(b),
        }
    }

    if !word.is_empty() {
        words.push(String::from_utf8(word).unwrap());
    }

    words
}

#[test]
#[cfg(target_os = "macos")]
fn system_library_mac_test() {
    assert!(!is_static_available("PluginManager", &[PathBuf::from("/Library/Frameworks")]));
    assert!(!is_static_available("python2.7", &[PathBuf::from("/System/Library/Frameworks/Python.framework/Versions/2.7/lib/python2.7/config")]));
    assert!(!is_static_available("ffi_convenience", &[PathBuf::from("/Library/Ruby/Gems/2.0.0/gems/ffi-1.9.10/ext/ffi_c/libffi-x86_64/.libs")]));

    // Homebrew is in /usr/local, and it's not a part of the OS
    if Path::new("/usr/local/lib/libpng16.a").exists() {
        assert!(is_static_available("png16", &[PathBuf::from("/usr/local/lib")]));
    }
}

#[test]
#[cfg(target_os = "linux")]
fn system_library_linux_test() {
    assert!(!is_static_available("util", &[PathBuf::from("/usr/lib/x86_64-linux-gnu")]));
    assert!(!is_static_available("dialog", &[PathBuf::from("/usr/lib")]));
}
