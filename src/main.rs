use eyre::{bail, Result};
use inquire::{list_option::ListOption, Select};
use log::{error, info};
use notify::{recommended_watcher, EventKind, RecursiveMode, Watcher};
use request::{batch_requests, make_request};
use std::env::current_dir;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use walkdir::WalkDir;

mod cli;
mod env;
mod logging;
mod request;
mod util;
use env::{find_root_dir, load_env, select_env};

mod extract;

mod substitute;

mod prompt;
use prompt::set_interactive_mode;

#[tokio::main]
async fn main() -> Result<()> {
    let args = cli::parse_args();

    logging::init(args.verbose, args.quiet, args.batch.is_some())?;

    set_interactive_mode(!(args.non_interactive || args.watch));

    let Some(root_dir) = find_root_dir()? else {
        bail!("No hitman.toml found");
    };

    if args.select {
        select_env(&root_dir)?;
        return Ok(());
    }

    let cwd = current_dir()?;

    let result = if let Some(file_path) = args.name {
        let file_path = cwd.join(file_path);
        let env = load_env(&root_dir, &file_path, &args.options)?;

        if let Some(batch) = args.batch {
            batch_requests(&file_path, batch, &env).await
        } else {
            let res = make_request(&file_path, &env).await;

            if args.watch {
                watch_mode(file_path, env).await
            } else {
                res
            }
        }
    } else {
        loop {
            let files = find_available_requests(&cwd)?;
            let options: Vec<ListOption<String>> = files
                .iter()
                .enumerate()
                .map(|(i, p)| ListOption::new(i, p.display().to_string()))
                .collect::<Vec<_>>();

            eprintln!();
            let selected = Select::new("Select request", options)
                .with_filter(&|filter, _, value, _| prompt::fuzzy_match(filter, value))
                .with_page_size(15)
                .prompt()?;

            let file_path = &files[selected.index];

            let env = load_env(&root_dir, file_path, &args.options)?;

            let result = make_request(&cwd.join(file_path), &env).await;
            if !args.repeat {
                break result;
            }

            match result {
                Ok(()) => (),
                Err(e) => {
                    if !is_user_cancelation(&e) {
                        error!("{}", e);
                    }
                }
            }
        }
    };

    // FIXME Must be a way to make this nicer
    match &result {
        Err(e) => {
            if is_user_cancelation(e) {
                Ok(())
            } else {
                result
            }
        }
        _ => result,
    }
}

fn is_user_cancelation(err: &eyre::Report) -> bool {
    matches!(
        err.downcast_ref(),
        Some(inquire::InquireError::OperationCanceled)
    ) || matches!(
        err.downcast_ref(),
        Some(inquire::InquireError::OperationInterrupted)
    )
}

fn find_available_requests(cwd: &Path) -> Result<Vec<PathBuf>> {
    let files: Vec<_> = WalkDir::new(cwd)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|s| s.ends_with(".http"))
                .unwrap_or(false)
        })
        .map(|p| {
            // Convert to relative path, based on depth
            let components: Vec<_> = p.path().components().collect();
            let relative_components: Vec<_> = components[(components.len() - p.depth())..]
                .iter()
                .map(|c| c.as_os_str())
                .collect();

            PathBuf::from_iter(&relative_components)
        })
        .collect();

    Ok(files)
}

async fn watch_mode(file_path: PathBuf, env: toml::Table) -> Result<()> {
    let (tx, mut rx) = mpsc::channel(1);

    let mut watcher = recommended_watcher(move |res| {
        if let Ok(event) = res {
            tx.blocking_send(event).expect("send to channel");
        }
    })?;

    watcher.watch(&file_path, RecursiveMode::NonRecursive)?;

    loop {
        info!("# Watching for changes...");
        if let Some(event) = rx.recv().await {
            if let EventKind::Modify(_) = event.kind {
                if let Err(err) = make_request(&file_path, &env).await {
                    error!("# {}", err)
                }
            }
        }
    }
}
