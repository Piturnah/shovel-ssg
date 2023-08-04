use std::{
    cell::OnceCell,
    collections::HashMap,
    fs::{self, File},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
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
}

impl Files {
    fn collect(path: &str) -> Result<Self> {
        let mut html = Vec::new();
        let mut components = HashMap::new();
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
            }
        }

        let root = path.to_string();
        Ok(Self {
            root,
            html,
            components,
        })
    }

    fn build(&self, path: &str) -> Result<()> {
        let component_slot_re = Regex::new(r"<\s*#([^,\s]+)\s*/>")?;
        for file in &self.html {
            let relative_path = file.path().strip_prefix(&self.root)?;
            let mut output_path = PathBuf::new();
            output_path.push(path);
            output_path.push(relative_path);
            let parent = output_path.parent().context("failed to get file parent")?;
            fs::create_dir_all(parent).context("failed to create parent directory")?;
            let mut buf = BufWriter::new(
                File::create(output_path).context("failed to open file for writing")?,
            );
            let content = fs::read_to_string(file.path()).unwrap();
            let new_content = component_slot_re.replace_all(&content, |captures: &Captures| {
                self.components.get(&captures[1]).unwrap().get_content()
            });
            let _ = buf.write(new_content.as_bytes())?;
        }

        Ok(())
    }
}

fn main() -> Result<()> {
    let files = Files::collect("test")?;
    files.build("build")
}
