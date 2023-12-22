use std::{
    ffi::{OsStr, OsString},
    fs::{self, read_to_string, rename},
    io::Write,
    path::{Path, PathBuf},
    str::FromStr,
    thread::{self, available_parallelism},
};

use anyhow::{anyhow, bail, Context};
use bpaf::{Bpaf, Parser};
use crossbeam_channel::Sender;
use ignore::{DirEntry, WalkBuilder, WalkState};
use tree_magic_mini::from_filepath;
use xshell::{cmd, Shell};

// bpaf docs: https://docs.rs/bpaf/latest/bpaf/index.html
// xshell docs: https://docs.rs/xshell/latest/xshell/index.html

#[derive(Debug, Clone)]
pub enum InputDir {
    Absolute(PathBuf),
    Relative(PathBuf),
}

impl FromStr for InputDir {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<InputDir> {
        let path = PathBuf::from_str(s).with_context(|| {
            "...while converting input argument into a PathBuf"
        })?;
        if path.is_absolute() {
            Ok(InputDir::Absolute(path))
        } else if path.is_relative() {
            Ok(InputDir::Relative(path))
        } else {
            bail!(
                "Input {} is not an absolute or relative path.",
                path.display()
            )
        }
    }
}

impl InputDir {
    fn canonicalize(&self, cwd: &Path) -> anyhow::Result<Directory> {
        let path = match self {
            InputDir::Absolute(p) => p.clone().canonicalize(),
            InputDir::Relative(p) => cwd.join(p).canonicalize(),
        }
        .with_context(|| {
            format!("...failed to canonicalize input path {:?}", self)
        })?;
        let name = path
            .iter()
            .last()
            .with_context(|| {
                format!("Input path {} is empty!", path.display())
            })?
            .into();
        if path.exists() && !path.is_dir() {
            Err(anyhow!(
                "Path {} already exists, but it is not a directory.",
                path.display()
            ))
        } else {
            Ok(Directory { path, name })
        }
    }
}

struct Directory {
    path: PathBuf,
    name: OsString,
}

impl AsRef<OsStr> for Directory {
    fn as_ref(&self) -> &OsStr {
        self.path.as_os_str()
    }
}

/// Rename a project, and its GH repository if one exists.
#[derive(Bpaf, Debug, Clone)]
struct Pmv {
    /// Example of a positional argument.
    #[bpaf(positional("PROJECT_PATH"))]
    existing: InputDir,

    /// New project name.
    #[bpaf(positional("NEW_NAME"))]
    new: String,
}

fn is_file(dir_entry: &DirEntry) -> bool {
    dir_entry.file_type().is_some_and(|fty| fty.is_file())
}

fn is_text_file(dir_entry: &DirEntry) -> bool {
    is_file(dir_entry)
        && from_filepath(dir_entry.path())
            .is_some_and(|mime| mime.contains("text"))
}

fn collect_if_text_file(
    tx: &Sender<Result<PathBuf, ignore::Error>>,
    result: Result<DirEntry, ignore::Error>,
) -> WalkState {
    if let Some(payload) = result
        .map(|dir_entry| {
            if is_text_file(&dir_entry) {
                Some(dir_entry.into_path())
            } else {
                None
            }
        })
        .transpose()
    {
        tx.send(payload).unwrap();
    }
    WalkState::Continue
}

fn find_and_replace_in_dir(
    dir: PathBuf,
    from: &str,
    to: &str,
) -> anyhow::Result<()> {
    let (tx, rx) =
        crossbeam_channel::bounded::<Result<PathBuf, ignore::Error>>(100);

    let n_cores = match available_parallelism() {
        Ok(n_cores) => n_cores.get(),
        _ => 1,
    };

    let collector = thread::spawn(move || {
        let mut file_paths = Vec::new();
        let mut stdout = std::io::BufWriter::new(std::io::stdout());
        for path in rx {
            let msg = match path {
                Ok(path) => {
                    let msg = format!("Renaming: {}\n", path.display());
                    file_paths.push(path);
                    msg
                }
                Err(err) => format!("{err}\n"),
            };
            stdout.write_all(msg.as_bytes()).unwrap();
        }
        file_paths
    });

    if n_cores > 1 {
        let walker = WalkBuilder::new(dir).threads(n_cores).build_parallel();
        walker.run(|| {
            let tx = tx.clone();
            Box::new(move |result| collect_if_text_file(&tx, result))
        });
    } else {
        let walker = WalkBuilder::new(dir).build();
        for result in walker {
            let _ = collect_if_text_file(&tx, result);
        }
    }
    drop(tx);
    let file_paths = collector.join().unwrap();

    for fp in file_paths {
        let contents = read_to_string(&fp).with_context(|| {
            format!("Failed to open and read text file: {}", fp.display())
        })?;
        fs::write(&fp, contents.replace(from, to)).with_context(|| {
            format!(
                "Could not write back new contents of file: {}",
                fp.display()
            )
        })?;
    }

    Ok(())
}

fn main() -> anyhow::Result<()> {
    let Pmv { existing, new } = pmv().run();

    let sh = Shell::new()?;
    let cwd = sh.current_dir();
    let existing = existing.canonicalize(&cwd)?;
    let new_path = existing
        .path
        .parent()
        .map(|parent| parent.to_path_buf())
        .unwrap_or_else(|| existing.path.clone())
        .join(&new);

    if new_path.exists() {
        bail!("{} already exists!", new_path.display());
    } else if existing.path == new_path {
        println!("New path is the same as current path.")
    }

    println!(
        "Moving from {} to {}.",
        existing.path.display(),
        new_path.display()
    );
    rename(&existing.path, &new_path).with_context(|| {
        format!(
            "Failed to rename {} to {}.",
            existing.path.display(),
            new_path.display(),
        )
    })?;

    sh.change_dir(&new_path);

    let old_name = existing.name.clone();
    find_and_replace_in_dir(
        sh.current_dir(),
        old_name.to_str().ok_or(anyhow!(
            "Could not convert name of the existing project () to a string."
        ))?,
        &new,
    )?;

    sh.change_dir(&new_path);
    if let Err(err) = cmd!(sh, "gh repo rename {new} --yes").run() {
        println!("Error creating a GitHub repo: {err}");
    }

    Ok(())
}
