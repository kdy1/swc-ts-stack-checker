use anyhow::{bail, Context, Result};
use futures_util::{future::BoxFuture, FutureExt};
use github_rs::client::{Executor, Github};
use serde::Deserialize;
use std::{env, path::Path};
use swc_common::{
    errors::{ColorConfig, Handler},
    input::StringInput,
    sync::Lrc,
    SourceMap,
};
use swc_ecma_parser::{lexer::Lexer, Parser, Syntax};
use tempfile::TempDir;
use tokio::{fs::read_dir, process::Command, spawn, task::spawn_blocking};

#[tokio::main]
async fn main() -> Result<()> {
    let fetcher = Fetcher {
        token: env::var("GITHUB_TOKEN").expect("Environment variable `GITHUB_TOKEN` is required"),
    };

    let args = env::args();
    let repos = if args.len() != 1 {
        let mut repos = vec![];
        let mut tasks = vec![];
        for arg in args.into_iter().skip(1) {
            tasks.push(fetcher.repos_of_org(arg));
        }
        for task in tasks {
            repos.push(task.await?);
        }
        repos.into_iter().flatten().collect()
    } else {
        fetcher.list_repositories().await?
    };

    let mut tasks = vec![];

    for repo in repos {
        let task = spawn(async move { handle(repo).await });
        tasks.push(task);
    }

    for task in tasks {
        task.await??;
    }

    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
struct Org {
    pub repos_url: String,
    pub login: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Repo {
    pub fork: bool,
    pub archived: bool,
    pub clone_url: String,
}

#[derive(Clone)]
struct Fetcher {
    token: String,
}

impl Fetcher {
    pub async fn repos_of_org(&self, name: String) -> Result<Vec<Repo>> {
        let token = self.token.clone();

        spawn_blocking(move || {
            eprintln!("Organization: {}", name);

            let client = Github::new(token).unwrap();

            let (_, _, repos) = match client
                .get()
                .orgs()
                .org(&name)
                .repos()
                .execute::<Vec<Repo>>()
            {
                Ok(v) => v,
                Err(err) => bail!("failed to fetch repository of organizations: {:?}", err),
            };

            Ok(repos.unwrap_or_default())
        })
        .await?
    }

    pub async fn list_repositories(&self) -> Result<Vec<Repo>> {
        let token = self.token.clone();

        let orgs = spawn_blocking(move || -> Result<_> {
            let client = Github::new(token).unwrap();
            let orgs = client.get().organizations().execute::<Vec<Org>>();
            let (_, _, orgs) = match orgs {
                Ok(v) => v,
                Err(err) => bail!("failed to fetch oranizations: {:?}", err),
            };

            Ok(orgs)
        })
        .await??;

        let mut buf = vec![];

        if let Some(orgs) = orgs {
            for org in orgs {
                let repos = self.repos_of_org(org.login).await?;
                buf.extend(
                    repos
                        .into_iter()
                        .filter(|repo| !repo.archived && !repo.fork),
                );
            }
        }

        Ok(buf)
    }
}

async fn handle(repo: Repo) -> Result<()> {
    let dir = git_pull(&repo).await?;
    check_all_files(dir.path()).await?;
    Ok(())
}

async fn git_pull(repo: &Repo) -> Result<TempDir> {
    eprintln!("Pulling {}", repo.clone_url);

    let cur_dir = env::current_dir().context("failed to get current directory")?;
    let tmp_dir = TempDir::new_in(&cur_dir.join(".data"))?;

    Command::new("git")
        .arg("pull")
        .arg("--depth")
        .arg("1")
        .arg(&repo.clone_url)
        .arg(tmp_dir.path())
        .output()
        .await
        .with_context(|| format!("failed to clone {}", repo.clone_url))?;

    Ok(tmp_dir)
}

fn check_all_files(dir: &Path) -> BoxFuture<Result<()>> {
    async move {
        let mut entries = read_dir(dir)
            .await
            .with_context(|| format!("failed to read dir: {}", dir.display()))?;

        loop {
            let entry = entries.next_entry().await?;
            let entry = match entry {
                Some(v) => v,
                None => break,
            };

            let path = entry.path();
            let ty = entry.file_type().await?;
            if ty.is_dir() {
                check_all_files(&path).await?;
            } else if ty.is_file() && path.ends_with(".ts") {
                let path = path.clone();
                spawn_blocking(move || check_file(&path)).await??;
            }
        }

        Ok(())
    }
    .boxed()
}

fn check_file(path: &Path) -> Result<()> {
    let cm: Lrc<SourceMap> = Default::default();
    let handler = Handler::with_tty_emitter(ColorConfig::Auto, true, false, Some(cm.clone()));

    // Real usage
    // let fm = cm
    //     .load_file(Path::new("test.js"))
    //     .expect("failed to load test.js");

    let fm = cm
        .load_file(path)
        .with_context(|| format!("failed to load file: {}", path.display()))?;

    let lexer = Lexer::new(
        Syntax::Typescript(Default::default()),
        Default::default(),
        StringInput::from(&*fm),
        None,
    );

    let mut parser = Parser::new_from(lexer);

    let _module = parser
        .parse_typescript_module()
        .map_err(|e| e.into_diagnostic(&handler).emit())
        .expect("Failed to parse module.");

    Ok(())
}
