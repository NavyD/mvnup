use std::{
    collections::HashSet,
    fs::{remove_dir_all, remove_file},
    path::PathBuf,
    process::exit,
    sync::Arc,
};

use anyhow::{anyhow, bail, Error, Result};

use comfy_table::Table;
use directories::BaseDirs;
use futures_util::{future::join_all, try_join};
use glob::glob;
use log::{debug, info, trace, warn};
use mvnup::{
    site::{BinFile, Site},
    util::{extract, find_java_version, find_mvn_version, match_digests},
    CRATE_NAME,
};
use semver::{Version, VersionReq};
use structopt::StructOpt;
use tokio::fs as afs;
use tokio::sync::Mutex;
use url::Url;
use which::which;

#[tokio::main]
async fn main() -> Result<()> {
    let opt = Opt::from_args();
    opt.init_log()?;
    Program::new(opt)?.run().await;
    Ok(())
}

#[derive(Debug, StructOpt, Clone)]
pub struct Opt {
    #[structopt(long, short, default_value = "https://archive.apache.org/dist/")]
    mirror: Url,

    #[structopt(long, short, parse(from_occurrences))]
    verbose: u8,

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
}

#[derive(Debug, StructOpt, Clone)]
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

#[derive(Debug, StructOpt, Clone)]
struct InstallArgs {
    #[structopt(long, short)]
    path: PathBuf,

    #[structopt(long, short)]
    version: Option<String>,
}

#[derive(Debug, StructOpt, Clone)]
struct ListArgs {
    #[structopt(long, short, default_value = "5")]
    limit: usize,
}

struct Program {
    site: Site,
    versions: Arc<Mutex<Vec<Version>>>,
    opt: Opt,
    cache_dir: PathBuf,
    base_dir: BaseDirs,
}

impl Program {
    pub fn new(opt: Opt) -> Result<Self> {
        let basedir = BaseDirs::new().ok_or_else(|| anyhow!("not found base dir"))?;
        let cache_dir = basedir.cache_dir().join(CRATE_NAME);
        std::fs::create_dir_all(&cache_dir)?;
        Ok(Self {
            versions: Arc::new(Mutex::new(vec![])),
            site: Site::new(opt.mirror.clone()).expect("new site error"),
            opt,
            cache_dir,
            base_dir: basedir,
        })
    }

    pub async fn run(&self) {
        match &self.opt.commands {
            Some(Commands::List { args }) => {
                if let Err(e) = self.list(args).await {
                    eprintln!("list failed: {}", e);
                    exit(1);
                }
            }
            Some(Commands::Install { args }) => {
                if let Err(e) = self.install(args).await {
                    eprintln!("install failed: {}", e);
                    exit(1);
                }
            }
            Some(Commands::Uninstall) => {
                if let Err(e) = self.uninstall().await {
                    eprintln!("uninstall failed: {}", e);
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

    async fn uninstall(&self) -> Result<()> {
        let bin_link_path = which("mvn").map_err(|e| anyhow!("not found maven path: {}", e))?;
        if !bin_link_path.metadata()?.file_type().is_symlink() {
            anyhow!(
                "failed to uninstall: {} is not installed by {}",
                bin_link_path.display(),
                CRATE_NAME
            );
        }

        let bin_path = bin_link_path.read_link()?;
        let bin_home = bin_path
            .parent()
            .and_then(|p| p.parent())
            .ok_or_else(|| anyhow!("not found 2 parents dir for {}", bin_path.display()))?;

        println!("removing mvn home path: {}", bin_home.display());
        remove_dir_all(bin_home)?;

        println!("removing a link {}", bin_link_path.display());
        remove_file(bin_link_path)?;

        Ok(())
    }

    async fn install(&self, args: &InstallArgs) -> Result<()> {
        if let Ok(p) = which("mvn") {
            bail!(
                "already installed {} version of mvn: {}",
                find_mvn_version(p.as_path())?,
                p.display()
            );
        }

        let to_path = args.path.as_path();
        // check path
        if !to_path.exists() {
            info!("creating dir of install: {}", to_path.display());
            afs::create_dir_all(to_path).await?;
        } else if !to_path.is_dir() {
            bail!("{} is not a dir", to_path.display());
        } else if to_path.read_dir().map(|mut dir| dir.next().is_some())? {
            bail!("{} is not a empty dir", to_path.display());
        }

        // match mvn version
        let mvn_version = if let Some(ver_pat) = &args.version {
            self.match_version(ver_pat).await?
        } else {
            self.latest_version().await?
        };

        println!(
            "selected mvn version {} of installation to {}",
            mvn_version,
            to_path.display()
        );

        // select one
        let bins = self.site.fetch_bins(mvn_version).await?;
        let select_bin = self.choose_bin(&bins)?;

        // download
        let down_path = self.cache_dir.join(select_bin.filename());
        if down_path.is_file() && match_digests(down_path.as_path(), select_bin) {
            // cache
            println!("using cached file: {}", down_path.display());
        } else {
            println!("downloading {}", select_bin.filename());
            select_bin.download(down_path.as_path()).await?;
        }

        // extract to path
        extract(down_path.as_path(), to_path)?;

        // link to $PATH
        let exe_path = glob(&format!("{}/**/mvn", to_path.display()))
            .map_err::<Error, _>(Into::into)?
            .flatten()
            .next()
            .ok_or_else(|| anyhow!("not found mvn bin in {}", to_path.display()))?
            .canonicalize()?;
        #[cfg(target_os = "linux")]
        {
            if let Some(bin_path) = self.base_dir.executable_dir().map(|p| p.join("mvn")) {
                if !bin_path.exists() {
                    println!(
                        "creating link {} for {}",
                        bin_path.display(),
                        exe_path.display(),
                    );
                    std::os::unix::fs::symlink(exe_path, bin_path)?;
                    println!("installation successful. just type: mvn");
                    return Ok(());
                }
            }
        }
        println!(
            "installation successful. please add {} to your PATH",
            exe_path.display()
        );
        Ok(())
    }

    fn choose_bin<'a>(&self, bins: &'a [BinFile]) -> Result<&'a BinFile> {
        let tar_suffix = [".tar.gz", ".tar.bz2", ".tar.xz"]
            .into_iter()
            .collect::<HashSet<_>>();
        let has_tar = which("tar").is_ok();
        for bin in bins {
            if has_tar && tar_suffix.iter().any(|s| bin.filename().ends_with(s)) {
                return Ok(bin);
            }
        }
        bail!("not found a bin")
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
                debug!("found mvn path: {}", p.display());
                let cur_ver = find_mvn_version(p.clone())?;
                println!(
                    "found installed maven version: {}, path: {}",
                    cur_ver,
                    p.display()
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

    async fn match_version(&self, ver_pat: &str) -> Result<Version> {
        // check java version
        trace!("finding java version");
        let _java_ver = which("java")
            .map_err(Into::into)
            .and_then(find_java_version)
            .map_err(|e| anyhow!("failed to find java version: {}", e))?;
        // todo: match with java version

        let req = ver_pat.parse::<VersionReq>()?;
        self.versions()
            .await?
            .iter()
            .find(|ver| req.matches(ver))
            .cloned()
            .ok_or_else(|| anyhow!("not matched version for {}", ver_pat))
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

#[cfg(test)]
mod tests {
    use once_cell::sync::Lazy;
    use tempfile::{tempdir, TempDir};

    use super::*;

    static TEMP_CACHE_DIR: Lazy<TempDir> = Lazy::new(|| tempdir().unwrap());

    fn new(opt: &Opt) -> Result<Program> {
        let base_dir = BaseDirs::new().ok_or_else(|| anyhow!("not found base dir"))?;
        let cache_dir = TEMP_CACHE_DIR.path().join(CRATE_NAME);
        std::fs::create_dir_all(&cache_dir)?;
        Ok(Program {
            versions: Arc::new(Mutex::new(vec![])),
            site: Site::new(opt.mirror.clone()).expect("new site error"),
            opt: opt.clone(),
            cache_dir,
            base_dir,
        })
    }

    #[tokio::test]
    async fn test_match_version() -> Result<()> {
        let opt = Opt::from_iter([] as [&str; 0]);
        let p = new(&opt)?;
        let latest_ver = p.latest_version().await?;

        let res = p.match_version(&latest_ver.to_string()).await?;
        assert_eq!(res, latest_ver);

        let res = p.match_version(">= 3").await?;
        assert_eq!(res, latest_ver);

        let ver = "3.8.3";
        let res = p.match_version(&format!("<= {}", ver)).await?;
        assert_eq!(res, ver.parse()?);

        let ver = "3.8";
        let res = p.match_version(&format!("< {}", ver)).await?;
        assert_eq!(res, "3.6.3".parse()?);
        Ok(())
    }
}
