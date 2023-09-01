use std::{
    cell::OnceCell,
    collections::HashMap,
    fs::{self, File},
    io::{stderr, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use clap::Parser;
use ignore::{
    overrides::{Override, OverrideBuilder},
    DirEntry, WalkBuilder,
};
use log::{info, warn};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use regex::{Captures, Regex};

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
enum TemplateKind {
    Html,
    Markdown,
    Other,
}

#[derive(Debug)]
struct Template {
    kind: TemplateKind,
    file: DirEntry,
}

#[derive(Debug)]
struct Files {
    root: String,
    templates: Vec<Template>,
    components: HashMap<String, Component>,
}

impl Files {
    /// Collects the relevant files in the provided root directory.
    fn collect(path: &Path, ignore_overrides: Override) -> Result<Self> {
        let mut templates = Vec::new();
        let mut components = HashMap::new();
        // TODO: Walk does not need to be built each time we call the function. It will always be
        // the same for a given Files.
        for entry in WalkBuilder::new(path).overrides(ignore_overrides).build() {
            let entry = entry?;
            let Some(Some(name)) = entry.path().file_name().map(|name| name.to_str()) else {
                continue;
            };
            if let Some(stem) = name.strip_suffix(".component.html") {
                components.insert(stem.to_string(), Component::new(entry.path()));
            } else if entry
                .file_type()
                .with_context(|| format!("couldn't determine file type of `{:?}`", entry.path()))?
                .is_file()
            {
                templates.push(Template {
                    kind: match entry.path().extension().map(|ext| ext.to_str()) {
                        Some(Some("html")) => TemplateKind::Html,
                        Some(Some("md")) => TemplateKind::Markdown,
                        _ => TemplateKind::Other,
                    },
                    file: entry,
                })
            }
        }

        let root = path
            .to_str()
            .context("failed to convert path to string")?
            .to_string();
        Ok(Self {
            root,
            templates,
            components,
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
        // XXX: In future, we may need to switch to an actual parsing of the HTML rather than a
        // simple RE. For example, this may break in the case of inlined JavaScript which uses some
        // tag in a string.
        let component_slot_re = Regex::new(r"<\s*#([^,\s]+)\s*/>")?;
        for Template { file, kind } in &self.templates {
            match kind {
                TemplateKind::Html => {
                    let output_path = self.get_output_path(Path::new(path), file.path())?;
                    let mut buf =
                        File::create(output_path).context("failed to open file for writing")?;
                    let content = fs::read_to_string(file.path()).unwrap();
                    let new_content =
                        component_slot_re.replace_all(&content, |captures: &Captures| {
                            self.components.get(&captures[1]).unwrap().get_content()
                        });
                    let _ = buf.write(new_content.as_bytes())?;
                }
                TemplateKind::Markdown => {
                    let rendered = markdown::file_to_html(file.path()).expect("invalid markdown");
                    let mut output_path = self.get_output_path(Path::new(path), file.path())?;
                    output_path.set_extension("html");
                    let mut buf =
                        File::create(output_path).context("failed to open file for writing")?;
                    buf.write(rendered.as_bytes())?;
                }
                TemplateKind::Other => {
                    let output_path = self.get_output_path(Path::new(path), file.path())?;
                    fs::copy(file.path(), output_path)
                        .with_context(|| format!("failed to copy file `{:?}`", file.path()))?;
                }
            }
        }

        Ok(())
    }
}

#[derive(Parser)]
struct Clargs {
    #[clap(default_value = ".")]
    input_dir: String,

    #[clap(long, short, default_value = "_build", name = "OUTPUT_PATH")]
    output_dir: String,

    /// Watch the input directory for changes and automatically rebuild.
    #[clap(long, short)]
    watch: bool,
}

fn main() -> Result<()> {
    let clargs = Arc::new(Clargs::parse());
    let input_dir = Path::new(&clargs.input_dir);
    let overrides = OverrideBuilder::new(input_dir)
        .add(&format!("!{}", &clargs.output_dir))?
        .build()?;
    let files = Files::collect(input_dir, overrides.clone())?;
    files.build(&clargs.output_dir)?;

    if let Err(e) = simple_logger::SimpleLogger::new().init() {
        eprintln!("WARNING: Was not able to initialise logging: {e:?}");
    }

    if clargs.watch {
        let clargs = Arc::clone(&clargs);
        let mut watcher = notify::recommended_watcher(move |res: Result<Event, _>| {
            match res {
                Ok(ev)
                    if matches!(
                        ev.kind,
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                    ) =>
                {
                    info!("{:?} detected change: {:?}", ev.paths, ev.kind);
                    match Files::collect(Path::new(&clargs.input_dir), overrides.clone())
                        .map(|files| files.build(&clargs.output_dir))
                    {
                        Ok(_) => info!("rebuild succeeded"),
                        Err(e) => warn!("rebuild failed: {e}"),
                    }
                }
                Err(e) => warn!("error: {e:?}"),
                _ => {}
            }
            let _ = stderr().flush();
        })?;

        watcher.watch(input_dir, RecursiveMode::Recursive)?;
        std::thread::park();
    }

    Ok(())
}
