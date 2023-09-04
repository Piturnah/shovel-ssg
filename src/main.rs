use std::{
    cell::OnceCell,
    collections::HashMap,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use clap::Parser;
use ignore::{overrides::OverrideBuilder, DirEntry, WalkBuilder};
use regex::{Captures, Regex};

#[cfg(feature = "dev")]
use std::{
    io::stderr,
    net::{Ipv4Addr, TcpListener},
    sync::mpsc,
    sync::Arc,
    thread,
};

#[cfg(feature = "dev")]
use log::{info, warn};
#[cfg(feature = "dev")]
use notify::{Event, EventKind, RecursiveMode, Watcher};
#[cfg(feature = "dev")]
use tungstenite::Message;

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
    fn collect(path: &Path, output_dir: &str) -> Result<Self> {
        let mut templates = Vec::new();
        let mut components = HashMap::new();
        // TODO: Walk does not need to be built each time we call the function. It will always be
        // the same for a given Files.
        let overrides = OverrideBuilder::new(path)
            .add(&format!("!{output_dir}"))?
            .add("!.git")?
            .add("!.gitignore")?
            .add("!.github")?
            .build()?;
        for entry in WalkBuilder::new(path)
            .hidden(false)
            .overrides(overrides)
            .build()
        {
            let entry = entry?;
            let Some(name) = entry.path().file_name().and_then(|name| name.to_str()) else {
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
                    kind: match entry.path().extension().and_then(|ext| ext.to_str()) {
                        Some("html") => TemplateKind::Html,
                        Some("md") => TemplateKind::Markdown,
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

    fn build(&self, path: &str, use_ws: bool) -> Result<()> {
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
                    let mut new_content =
                        component_slot_re.replace_all(&content, |captures: &Captures| {
                            self.components.get(&captures[1]).unwrap().get_content()
                        });
                    if use_ws {
                        inject_websocket(new_content.to_mut());
                    }
                    let _ = buf.write(new_content.as_bytes())?;
                }
                TemplateKind::Markdown => {
                    let mut rendered =
                        markdown::file_to_html(file.path()).context("invalid markdown")?;
                    if use_ws {
                        inject_websocket(&mut rendered);
                    }
                    let mut output_path = self.get_output_path(Path::new(path), file.path())?;
                    output_path.set_extension("html");
                    let mut buf =
                        File::create(output_path).context("failed to open file for writing")?;
                    buf.write_all(rendered.as_bytes())?;
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

/// Inject some websocket code that will allow us to do the automatic refresh for the live server.
fn inject_websocket(html: &mut String) {
    let head_close_re = Regex::new(r"</\s*head\s*>").unwrap();
    let inject_idx = head_close_re.find(html).map_or(0, |mat| mat.start());
    html.insert_str(inject_idx, include_str!("./inject_ws.html"));
}

#[derive(Parser)]
struct Clargs {
    #[clap(default_value = ".")]
    input_dir: String,

    #[clap(long, short, default_value = "_build", name = "OUTPUT_PATH")]
    output_dir: String,

    /// Watch the input directory and rebuild on change.
    #[cfg(feature = "dev")]
    #[clap(long, short)]
    watch: bool,

    /// Serve the compiled site on localhost. Also enables `watch`.
    #[cfg(feature = "dev")]
    #[clap(long, short)]
    serve: bool,
}

fn run(clargs: &Clargs) -> Result<()> {
    if let Err(e) = simple_logger::SimpleLogger::new().init() {
        eprintln!("WARNING: Was not able to initialise logging: {e:?}");
    }

    let input_dir = Path::new(&clargs.input_dir);
    let files = Files::collect(input_dir, &clargs.output_dir)?;

    #[cfg(feature = "dev")]
    files.build(&clargs.output_dir, clargs.serve)?;

    #[cfg(not(feature = "dev"))]
    files.build(&clargs.output_dir, false)?;

    Ok(())
}

#[cfg(not(feature = "dev"))]
fn main() -> Result<()> {
    let clargs = Clargs::parse();
    run(&clargs)
}

#[cfg(feature = "dev")]
#[tokio::main]
async fn main() -> Result<()> {
    let mut clargs = Clargs::parse();
    clargs.watch |= clargs.serve;
    run(&clargs)?;

    let (request_tx, request_rx) = mpsc::channel();
    if clargs.serve {
        let server = TcpListener::bind((Ipv4Addr::LOCALHOST, 3031)).unwrap();
        thread::spawn(move || {
            for request in server.incoming().filter_map(|req| req.ok()) {
                let _ = request_tx.send(request);
            }
        });
    }

    if clargs.watch {
        let clargs = Arc::new(clargs);
        let clargs1 = Arc::clone(&clargs);
        let mut watcher = notify::recommended_watcher(move |res: Result<Event, _>| {
            match res {
                Ok(ev)
                    if matches!(
                        ev.kind,
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                    ) =>
                {
                    info!("{:?} detected change: {:?}", ev.paths, ev.kind);
                    match Files::collect(Path::new(&clargs1.input_dir), &clargs1.output_dir)
                        .map(|files| files.build(&clargs1.output_dir, clargs1.serve))
                    {
                        Ok(_) => {
                            info!("rebuild succeeded");
                            if clargs1.serve {
                                while let Ok(request) = request_rx.try_recv() {
                                    let message = Message::Text("reload".to_string());
                                    let _ = tungstenite::accept(request)
                                        .map(|mut socket| socket.send(message));
                                }
                            }
                        }
                        Err(e) => warn!("rebuild failed: {e}"),
                    }
                }
                Err(e) => warn!("error: {e:?}"),
                _ => {}
            }
            let _ = stderr().flush();
        })?;

        watcher.watch(Path::new(&clargs.input_dir), RecursiveMode::Recursive)?;
        if clargs.serve {
            warp::serve(warp::fs::dir(clargs.output_dir.clone()))
                .run(([127, 0, 0, 1], 3030))
                .await;
        }
        std::thread::park();
    }

    Ok(())
}
