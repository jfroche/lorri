//! Builds a nix derivation file (like a `shell.nix` file).
//!
//! It is a wrapper around `nix-build`.
//!
//! Note: this does not build the Nix expression as-is.
//! It instruments various nix builtins in a way that we
//! can parse additional information from the `nix-build`
//! `stderr`, like which source files are used by the evaluator.

use cas::ContentAddressable;
use regex::Regex;
use std::any::Any;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use vec1::Vec1;
use {DrvFile, NixFile, StorePath};

fn instrumented_instantiation(
    root_nix_file: &NixFile,
    cas: &ContentAddressable,
) -> Result<Info<OutputPaths<DrvFile>>, Error> {
    // We're looking for log lines matching:
    //
    //     copied source '...' -> '/nix/store/...'
    //     evaluating file '...'
    //
    // to determine which files we should setup watches on.
    // Increasing verbosity by two levels via `-vv` satisfies that.

    let mut cmd = Command::new("nix-instantiate");

    let logged_evaluation_nix = cas.file_from_string(include_str!("./logged-evaluation.nix"))?;

    // TODO: see ::nix::CallOpts::paths for the problem with this
    let gc_root_dir = tempfile::TempDir::new()?;

    cmd.args(&[
        // verbose mode prints the files we track
        OsStr::new("-vv"),
        // we add a temporary indirect GC root
        OsStr::new("--add-root"),
        gc_root_dir.path().join("result").as_os_str(),
        OsStr::new("--indirect"),
        OsStr::new("--argstr"),
        // runtime nix paths to needed dependencies that come with lorri
        OsStr::new("runTimeClosure"),
        OsStr::new(crate::RUN_TIME_CLOSURE),
        // the source file
        OsStr::new("--argstr"),
        OsStr::new("src"),
        root_nix_file.as_os_str(),
        // instrumented by `./logged-evaluation.nix`
        OsStr::new("--"),
        &logged_evaluation_nix.as_os_str(),
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    debug!("$ {:?}", cmd);

    let output = cmd.spawn()?.wait_with_output()?;

    let stderr_results =
        ::nix::parse_nix_output(&output.stderr, |line| parse_evaluation_line(line));

    let produced_drvs = Vec1::from_vec(::nix::parse_nix_output(&output.stdout, StorePath::from))
        // programming error
        .unwrap_or_else(|_| {
            panic!(
                "`lorri read` didn’t get a store path in its output:\n{:#?}",
                stderr_results.clone()
            )
        });

    // iterate over all lines, parsing out the ones we are interested in
    let (paths, output_paths, log_lines): (
        Vec<PathBuf>,
        // `None` if the field was not seen before, `Some` if it was
        OutputPaths<Option<DrvFile>>,
        Vec<OsString>
    ) =
    stderr_results.clone().into_iter().fold(
        (vec![], OutputPaths { shell: None, shell_gc_root: None }, vec![]),
        |(mut paths, mut output_paths, mut log_lines), result| {
                match result {
                    LogDatum::Source(src) => {
                        paths.push(src);
                    }
                    LogDatum::ShellDrv(drv) => {
                        // check whether we have seen this field before
                        match output_paths.shell {
                            None => { output_paths.shell = Some(DrvFile(drv)); }
                            // programming error
                            Some(DrvFile(old)) => panic!(
                                "`lorri read` got attribute `{}` a second time, first path was {:?} and second {:?}",
                                "shell", old, drv)
                        }
                    },
                    LogDatum::ShellGcRootDrv(drv) => {
                        // check whether we have seen this field before
                        match output_paths.shell_gc_root {
                            None => { output_paths.shell_gc_root = Some(DrvFile(drv)); }
                            // programming error
                            Some(DrvFile(old)) => panic!(
                                "`lorri read` got attribute `{}` a second time, first path was {:?} and second {:?}",
                                "shell_gc_root", old, drv)
                        }
                    },
                    LogDatum::Text(line) => log_lines.push(line),
                };

                (paths, output_paths, log_lines)
            },
        );

    if !output.status.success() {
        return Ok(Info::Failure(Failure {
            exec_result: output.status,
            log_lines,
        }));
    }

    // check whether we got all required `OutputPaths`
    let output_paths = match output_paths {
        // programming error
        OutputPaths { shell: None, .. } => panic!(
            "`lorri read` never got required attribute `shell:\n{:#?}`",
            stderr_results
        ),
        // programming error
        OutputPaths {
            shell_gc_root: None,
            ..
        } => panic!(
            "`lorri read` never got required attribute `shell_gc_root`\n{:#?}",
            stderr_results
        ),
        OutputPaths {
            shell: Some(shell),
            shell_gc_root: Some(shell_gc_root),
        } => OutputPaths {
            shell,
            shell_gc_root,
        },
    };

    Ok(Info::Success(Success {
        drvs: (produced_drvs, ::nix::GcRootTempDir(gc_root_dir)),
        output_paths,
        paths,
    }))
}

/// Builds the Nix expression in `root_nix_file`.
///
/// Instruments the nix file to gain extra information,
/// which is valuable even if the build fails.
pub fn run(root_nix_file: &NixFile, cas: &ContentAddressable) -> Result<Info<StorePath>, Error> {
    let inst_info = instrumented_instantiation(root_nix_file, cas)?;
    match inst_info {
        Info::Success(s) => {
            let drvs = s.output_paths.clone();
            // TODO: we are only using shell_gc_root here, I don’t think
            // we are using the shell anywhere anymore. Then we could remove
            // it from OutputPaths and simplify logged-evaluation.nix!
            let realized = ::nix::CallOpts::file(drvs.shell_gc_root.as_path()).path()?;
            match s {
                Success { paths, .. } => Ok(Info::Success(Success {
                    // TODO: duplication, remove drvs in favour of output_paths
                    drvs: (vec1::vec1![realized.0.clone()], realized.1),
                    output_paths: realized.0,
                    paths,
                })),
            }
        }
        Info::Failure(f) => Ok(Info::Failure(f)),
    }
}

#[derive(Debug, PartialEq, Clone)]
enum LogDatum {
    Source(PathBuf),
    ShellDrv(PathBuf),
    ShellGcRootDrv(PathBuf),
    Text(OsString),
}

/// Examine a line of output and extract interesting log items in to
/// structured data.
fn parse_evaluation_line(line: &OsStr) -> LogDatum {
    lazy_static! {
        static ref EVAL_FILE: Regex =
            Regex::new("^evaluating file '(?P<source>.*)'$").expect("invalid regex!");
        static ref COPIED_SOURCE: Regex =
            Regex::new("^copied source '(?P<source>.*)' -> '(?:.*)'$").expect("invalid regex!");
        static ref LORRI_READ: Regex =
            Regex::new("^trace: lorri read: '(?P<source>.*)'$").expect("invalid regex!");
        static ref LORRI_ATTR_DRV: Regex =
            Regex::new("^trace: lorri attribute: '(?P<attribute>.*)' -> '(?P<drv>/nix/store/.*)'$")
                .expect("invalid regex!");
    }

    match line.to_str() {
        // If we can’t decode the output line to an UTF-8 string,
        // we cannot match against regexes, so just pass it through.
        None => LogDatum::Text(line.to_owned()),
        Some(linestr) => {
            // Lines about evaluating a file are much more common, so looking
            // for them first will reduce comparisons.
            if let Some(matches) = EVAL_FILE.captures(&linestr) {
                LogDatum::Source(PathBuf::from(&matches["source"]))
            } else if let Some(matches) = COPIED_SOURCE.captures(&linestr) {
                LogDatum::Source(PathBuf::from(&matches["source"]))
            } else if let Some(matches) = LORRI_READ.captures(&linestr) {
                LogDatum::Source(PathBuf::from(&matches["source"]))
            } else if let Some(matches) = LORRI_ATTR_DRV.captures(&linestr) {
                let drv = &matches["drv"];
                let attr = &matches["attribute"];
                match attr {
                    "shell" => LogDatum::ShellDrv(PathBuf::from(drv)),
                    "shell_gc_root" => LogDatum::ShellGcRootDrv(PathBuf::from(drv)),
                    _ => panic!(
                        "`lorri read` trace was `{} -> {}`, unknown attribute `{}`! (add to `builder.rs`)",
                        attr, drv, attr
                    ),
                }
            } else {
                LogDatum::Text(line.to_owned())
            }
        }
    }
}

/// The results of an individual instantiation/build.
/// Even if the exit code is not 0, there is still
/// valuable information in the output, like new paths
/// to watch.
#[derive(Debug)]
pub enum Info<T> {
    /// Nix ran successfully.
    Success(Success<T>),
    /// Nix returned a failing status code.
    Failure(Failure),
}

/// A successful Nix run.
#[derive(Debug)]
pub struct Success<T> {
    /// See `OutputPaths`
    // TODO: move back to `OutputPaths<T>`
    pub output_paths: T,

    // TODO: this is redundant with shell_gc_root
    /// A list of the evaluation's result derivations
    pub drvs: (Vec1<StorePath>, ::nix::GcRootTempDir),

    // TODO: rename to `sources` (it’s the input sources we have to watch)
    /// A list of paths examined during the evaluation
    pub paths: Vec<PathBuf>,
}

/// A failing Nix run.
#[derive(Debug)]
pub struct Failure {
    /// The error status code
    exec_result: std::process::ExitStatus,

    /// A list of stderr log lines
    pub log_lines: Vec<OsString>,
}

/// Output derivations generated by `logged-evaluation.nix`
#[derive(Debug, Clone)]
pub struct OutputPaths<T> {
    /// Original shell derivation
    pub shell: T,
    /// Shell derivation modified to work as a gc root
    pub shell_gc_root: T,
}

/// Return the name of each `OutputPaths` attribute.
pub fn output_path_attr_names() -> OutputPaths<String> {
    OutputPaths {
        shell: String::from("shell"),
        shell_gc_root: String::from("shell_gc_root"),
    }
}

impl<T> OutputPaths<T> {
    /// `map` for `OutputPaths`.
    pub fn map<F, U>(self, f: F) -> OutputPaths<U>
    where
        F: Fn(T) -> U,
    {
        OutputPaths {
            shell: f(self.shell),
            shell_gc_root: f(self.shell_gc_root),
        }
    }

    /// Like `map`, but return the first `Err`.
    pub fn map_res<F, U, E>(self, f: F) -> Result<OutputPaths<U>, E>
    where
        F: Fn(T) -> Result<U, E>,
    {
        Ok(OutputPaths {
            shell: f(self.shell)?,
            shell_gc_root: f(self.shell_gc_root)?,
        })
    }

    /// `zip` for `OutputPaths`
    pub fn zip<U>(self, us: OutputPaths<U>) -> OutputPaths<(T, U)> {
        OutputPaths {
            shell: (self.shell, us.shell),
            shell_gc_root: (self.shell_gc_root, us.shell_gc_root),
        }
    }
}

/// Possible errors from an individual evaluation
#[derive(Debug)]
pub enum Error {
    /// Executing nix-instantiate failed
    Instantiate(std::io::Error),

    /// Executing nix-build failed
    Build(::nix::OnePathError),

    /// Failed to spawn a log processing thread
    ThreadFailure(std::boxed::Box<(dyn std::any::Any + std::marker::Send + 'static)>),
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::Instantiate(e)
    }
}
impl From<::nix::OnePathError> for Error {
    fn from(e: ::nix::OnePathError) -> Error {
        Error::Build(e)
    }
}
impl From<Box<dyn Any + Send + 'static>> for Error {
    fn from(e: std::boxed::Box<(dyn std::any::Any + std::marker::Send + 'static)>) -> Error {
        Error::ThreadFailure(e)
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_evaluation_line, LogDatum};
    use std::ffi::OsString;
    use std::path::PathBuf;

    #[test]
    fn test_evaluation_line_to_path_evaluation() {
        assert_eq!(
            parse_evaluation_line(&OsString::from("evaluating file '/nix/store/zqxha3ax0w771jf25qdblakka83660gr-source/lib/systems/for-meta.nix'")),
            LogDatum::Source(PathBuf::from("/nix/store/zqxha3ax0w771jf25qdblakka83660gr-source/lib/systems/for-meta.nix"))
        );

        assert_eq!(
            parse_evaluation_line(&OsString::from("copied source '/nix/store/zqxha3ax0w771jf25qdblakka83660gr-source/pkgs/stdenv/generic/default-builder.sh' -> '/nix/store/9krlzvny65gdc8s7kpb6lkx8cd02c25b-default-builder.sh'")),
            LogDatum::Source(PathBuf::from("/nix/store/zqxha3ax0w771jf25qdblakka83660gr-source/pkgs/stdenv/generic/default-builder.sh"))
        );

        assert_eq!(
            parse_evaluation_line(&OsString::from(
                "trace: lorri read: '/home/grahamc/projects/grahamc/lorri/nix/nixpkgs.json'"
            )),
            LogDatum::Source(PathBuf::from(
                "/home/grahamc/projects/grahamc/lorri/nix/nixpkgs.json"
            ))
        );

        assert_eq!(
            parse_evaluation_line(&OsString::from("trace: lorri attribute: 'shell' -> '/nix/store/q3ngidzvincycjjvlilf1z6vj1w4wnas-lorri.drv'")),
            LogDatum::ShellDrv(PathBuf::from("/nix/store/q3ngidzvincycjjvlilf1z6vj1w4wnas-lorri.drv"))
        );
        assert_eq!(
            parse_evaluation_line(&OsString::from("trace: lorri attribute: 'shell_gc_root' -> '/nix/store/q3ngidzvincycjjvlilf1z6vj1w4wnas-lorri-keep-env-hack-foo.drv'")),
            LogDatum::ShellGcRootDrv(PathBuf::from("/nix/store/q3ngidzvincycjjvlilf1z6vj1w4wnas-lorri-keep-env-hack-foo.drv"))
        );

        assert_eq!(
            parse_evaluation_line(&OsString::from(
                "downloading 'https://static.rust-lang.org/dist/channel-rust-stable.toml'..."
            )),
            LogDatum::Text(OsString::from(
                "downloading 'https://static.rust-lang.org/dist/channel-rust-stable.toml'..."
            ))
        );
    }

    #[test]
    fn transitive_source_file_detection() -> std::io::Result<()> {
        Ok(())
    }
}