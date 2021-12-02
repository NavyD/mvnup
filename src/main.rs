use std::{
    collections::HashMap,
    fmt::Display,
    io::Write,
    path::{Path, PathBuf},
    process::exit,
    str::FromStr,
    sync::Arc,
};

use cmd_lib::{run_cmd, run_fun};
use comfy_table::Table;
use mvnup::{
    site::{BinFile, Site},
    util::find_mvn_version,
};
use once_cell::sync::Lazy;
use regex::Regex;
use semver::Version;
use tokio::{fs as afs, sync::Mutex};

use anyhow::{anyhow, bail, Error, Result};
use chrono::{Date, DateTime, Local};
use futures_util::{future::join_all, try_join, StreamExt};
use log::{debug, error, info, trace, warn};
use scraper::{Html, Selector};
use structopt::StructOpt;
use tempfile::tempfile;
use tokio::io::AsyncWriteExt;
use url::{ParseError, Url};
use which::which;

#[tokio::main]
async fn main() -> Result<()> {
    let opt = Opt::from_args();
    opt.init_log()?;
    Program::new(opt).run().await;
    Ok(())
}

#[derive(Debug, StructOpt)]
pub struct Opt {
    #[structopt(long, short, default_value = "https://archive.apache.org/dist/")]
    mirror: Url,

    #[structopt(long, short, parse(from_occurrences))]
    verbose: u8,

    #[structopt(long, short)]
    list: bool,

    #[structopt(subcommand)]
    commands: Option<Commands>,
}

impl Opt {
    fn init_log(&self) -> Result<()> {
        let verbose = self.verbose;
        if verbose > 4 {
            return Err(anyhow!("invalid arg: 4 < {} number of verbose", verbose));
        }
        let level: log::LevelFilter = unsafe { std::mem::transmute((verbose + 1) as usize) };
        env_logger::builder()
            .filter_level(log::LevelFilter::Error)
            .filter_module(module_path!(), level)
            .init();
        Ok(())
    }

    async fn run(&self) -> Result<()> {
        todo!()
    }
}

#[derive(Debug, StructOpt)]
enum Commands {
    Install {
        #[structopt(flatten)]
        args: InstallArgs,
    },
    Update {
        version: String,
    },
    Uninstall,
    List {
        #[structopt(flatten)]
        args: ListArgs,
    },
}

#[derive(Debug, StructOpt)]
struct InstallArgs {
    #[structopt(long, short, default_value = "")]
    path: PathBuf,

    #[structopt(long, short, default_value = "")]
    version: String,
}

#[derive(Debug, StructOpt)]
struct ListArgs {
    #[structopt(long, short, default_value = "5")]
    limit: usize,
}

struct Program {
    site: Site,
    versions: Arc<Mutex<Vec<Version>>>,
    opt: Opt,
}

impl Program {
    pub fn new(opt: Opt) -> Self {
        Self {
            versions: Arc::new(Mutex::new(vec![])),
            site: Site::new(opt.mirror.clone()).expect("new site error"),
            opt,
        }
    }

    pub async fn run(&self) {
        match &self.opt.commands {
            Some(Commands::List { args }) => {
                if let Err(e) = self.list(args).await {
                    eprintln!("list failed: {}", e);
                    exit(1);
                }
            }
            None => {
                if let Err(e) = self.check().await {
                    eprintln!("check failed: {}", e);
                    exit(1);
                }
            }
            _ => {
                eprintln!("unsupported");
                exit(1)
            }
        }
    }

    fn install(&self, args: &InstallArgs) -> Result<()> {
        todo!()
    }

    async fn list(&self, args: &ListArgs) -> Result<()> {
        let vers = self.versions().await?;
        let limit = if vers.len() < args.limit {
            vers.len()
        } else {
            args.limit
        };
        println!("fetching info of {} versions:", limit);
        let bins = self.get_multi_bins(&vers[..limit]).await?;

        let mut table = Table::new();
        table.set_header(vec!["version", "pulished date", "filename", "size: MB"]);
        for (ver, files) in bins {
            for file in files {
                let size = (*file.size() as f64 / (1024.0 * 1024.0))
                    .to_string()
                    .chars()
                    .take(6)
                    .collect::<String>();
                table.add_row(vec![
                    &ver.to_string(),
                    &file.last_modified().to_string(),
                    file.filename(),
                    &size,
                ]);
            }
        }
        println!("{}", table);
        println!(
            "...A total of {} versions were found and {} were filtered",
            vers.len(),
            vers.len() - limit
        );
        Ok(())
    }

    async fn check(&self) -> Result<()> {
        // check if mvn is installed
        match which("mvn") {
            Ok(p) => {
                let path_str = p.to_str().expect("to str error");
                debug!("found mvn path: {}", path_str);

                let cur_ver = find_mvn_version(p.clone())?;
                println!(
                    "found installed maven version: {}, path: {}",
                    cur_ver, path_str
                );
                let latest_ver = self.latest_version().await?;
                let (cur_date, latest_date) = try_join!(
                    self.site.fetch_bins(cur_ver.clone()),
                    self.site.fetch_bins(latest_ver.clone())
                )
                .map(|(cur_bins, latest_bins)| {
                    (
                        cur_bins[0].last_modified().date().to_string(),
                        latest_bins[0].last_modified().date().to_string(),
                    )
                })?;
                // let bins = self.site.fetch_bins(latest_ver).await?;

                use std::cmp::Ordering::*;
                match cur_ver.cmp(&latest_ver) {
                    Equal => {
                        println!("up to date: {} ({})", cur_ver, cur_date,);
                    }
                    Less => {
                        println!(
                            "update available: {} ({}) -> {} ({})",
                            cur_ver, cur_date, latest_ver, latest_date
                        );
                    }
                    Greater => {
                        bail!(
                            "failure version. installed: {}. latest: {}",
                            cur_ver,
                            latest_ver
                        );
                    }
                }
            }
            Err(e) => {
                bail!("not found mvn: {}", e);
            }
        }
        Ok(())
    }

    async fn versions(&self) -> Result<Vec<Version>> {
        let mut vers = self.versions.lock().await;
        if !vers.is_empty() {
            return Ok(vers.to_vec());
        }
        *vers = self.site.fetch_versions().await?;
        vers.sort_unstable_by(|a, b| b.cmp(a));
        Ok(vers.to_vec())
    }

    async fn latest_version(&self) -> Result<Version> {
        self.versions()
            .await
            .map(|vers| vers.first().cloned().unwrap())
    }

    async fn get_multi_bins(&self, versions: &[Version]) -> Result<Vec<(Version, Vec<BinFile>)>> {
        trace!("fetching bins with {} tasks", versions.len());
        let res = join_all(versions.iter().map(|ver| {
            let ver = ver.clone();
            async move {
                let ver_str = ver.to_string();
                self.site
                    .fetch_bins(ver.clone())
                    .await
                    .map(|bins| (ver, bins))
                    .map_err(|e| {
                        warn!("failed to fetch bins for version {}: {}", ver_str, e);
                        e
                    })
            }
        }))
        .await
        .into_iter()
        .flatten()
        .collect::<Vec<(Version, Vec<BinFile>)>>();
        if res.is_empty() {
            bail!("not found any bins");
        }
        Ok(res)
    }
}

// pub async fn download(&self, ver: &str) -> Result<afs::File> {
//     let url = self.zip_bin_url(ver);
//     let resp = reqwest::get(url).await?;
//     let mut stream = resp.bytes_stream();
//     let mut tmpfile = afs::File::from_std(tempfile()?);
//     while let Some(chunk) = stream.next().await {
//         let mut chunk = chunk?;
//         tmpfile.write_all_buf(&mut chunk).await?;
//     }
//     Ok(tmpfile)
// }
