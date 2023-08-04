use std::{
    cell::OnceCell,
    collections::HashMap,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use clap::Parser;
use regex::{Captures, Regex};
use walkdir::{DirEntry, WalkDir};

#[derive(Debug)]
struct Component {
    path: Box<Path>,
    content: OnceCell<String>,
}

impl Component {
    fn new(path: &Path) -> Self {
        Self {
            path: path.into(),
            content: OnceCell::new(),
        }
    }

    /// Get plaintext content of the component.
    fn get_content(&self) -> &String {
        self.content
            .get_or_init(|| fs::read_to_string(&self.path).unwrap())
    }
}

#[derive(Debug)]
struct Files {
    root: String,
    html: Vec<DirEntry>,
    components: HashMap<String, Component>,
    other: Vec<DirEntry>,
}

impl Files {
    fn collect(path: &str) -> Result<Self> {
        let mut html = Vec::new();
        let mut components = HashMap::new();
        let mut other = Vec::new();
        for entry in WalkDir::new(path) {
            let entry = entry?;
            let name = match entry.path().file_name() {
                Some(name) => name.to_str().context("failed to read file path")?,
                None => continue,
            };
            if let Some(stem) = name.strip_suffix(".component.html") {
                components.insert(stem.to_string(), Component::new(entry.path()));
            } else if let Some(Some("html")) = entry.path().extension().map(|ext| ext.to_str()) {
                html.push(entry);
            } else if entry.file_type().is_file() {
                other.push(entry);
            }
        }

        let root = path.to_string();
        Ok(Self {
            root,
            html,
            components,
            other,
        })
    }

    /// Give the path of an input file and it will create the relevant directory tree in the output
    /// directory, returning a `PathBuf` to where the output file will go.
    fn get_output_path(&self, build_path: &Path, file_path: &Path) -> Result<PathBuf> {
        let relative_path = file_path.strip_prefix(&self.root)?;
        let mut output_path = PathBuf::new();
        output_path.push(build_path);
        output_path.push(relative_path);
        let parent = output_path.parent().context("failed to get file parent")?;
        fs::create_dir_all(parent).context("failed to create parent directory")?;
        Ok(output_path)
    }

    fn build(&self, path: &str) -> Result<()> {
        let component_slot_re = Regex::new(r"<\s*#([^,\s]+)\s*/>")?;
        for file in &self.html {
            let output_path = self.get_output_path(Path::new(path), file.path())?;
            let mut buf = File::create(output_path).context("failed to open file for writing")?;
            let content = fs::read_to_string(file.path()).unwrap();
            let new_content = component_slot_re.replace_all(&content, |captures: &Captures| {
                self.components.get(&captures[1]).unwrap().get_content()
            });
            let _ = buf.write(new_content.as_bytes())?;
        }
        for file in &self.other {
            let output_path = self.get_output_path(Path::new(path), file.path())?;
            fs::copy(file.path(), output_path)
                .with_context(|| format!("failed to copy file `{:?}`", file.path()))?;
        }

        Ok(())
    }
}

#[derive(Parser)]
struct Clargs {
    #[clap(default_value = ".")]
    input_dir: String,

    #[clap(long, short, default_value = "build", name = "PATH")]
    output_dir: String,
}

fn main() -> Result<()> {
    let clargs = Clargs::parse();
    let files = Files::collect(&clargs.input_dir)?;
    files.build(&clargs.output_dir)
}
