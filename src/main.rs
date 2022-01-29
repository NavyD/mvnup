use std::{
    collections::HashSet,
    fs::{remove_dir_all, remove_file},
    path::PathBuf,
    process::exit,
    sync::Arc,
};

use anyhow::{anyhow, bail, Error, Result};
use comfy_table::Table;
use directories::{BaseDirs, ProjectDirs};
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
        #[structopt(long, short)]
        version: Option<String>,
    },
    Update {
        version: Option<String>,
    },
    Uninstall,
    List {
        #[structopt(long, short, default_value = "5")]
        limit: usize,
    },
}

struct Program {
    opt: Opt,
    manager: Manager,
    base_dir: BaseDirs,
    project_dirs: ProjectDirs,
}

impl Program {
    pub fn new(opt: Opt) -> Result<Self> {
        let base_dir = BaseDirs::new().ok_or_else(|| anyhow!("not found base dir"))?;
        Ok(Self {
            manager: Manager::new(Site::new(opt.mirror.clone()).expect("new site error"))?,
            opt,
            base_dir,
            project_dirs: ProjectDirs::from("xyz", "navyd", CRATE_NAME)
                .ok_or_else(|| anyhow!("project dir error"))?,
        })
    }

    pub async fn run(&self) {
        match &self.opt.commands {
            Some(Commands::List { limit }) => {
                if let Err(e) = self.list(*limit).await {
                    eprintln!("list failed: {}", e);
                    exit(1);
                }
            }
            Some(Commands::Install { version }) => {
                if let Err(e) = self.install(version.as_deref()).await {
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
            Some(Commands::Update { version }) => {
                if let Err(e) = self.update(version.as_deref()).await {
                    eprintln!("update failed: {}", e);
                    exit(1);
                }
            }
            None => {
                if let Err(e) = self.check().await {
                    eprintln!("check failed: {}", e);
                    exit(1);
                }
            }
        }
    }

    async fn update(&self, version: Option<&str>) -> Result<()> {
        let bin_link_path = which("mvn")?;
        if !bin_link_path.symlink_metadata()?.file_type().is_symlink() {
            bail!(
                "not found mvn installed path for bin: {}",
                bin_link_path.display()
            );
        }
        let bin_path = bin_link_path.read_link()?;

        let installed_ver = find_mvn_version(&bin_path)?;
        let ver = if let Some(ver_pat) = version {
            self.manager.match_version(ver_pat).await?
        } else {
            self.manager.latest_version().await?
        };

        let mvn_path = match installed_ver.cmp(&ver) {
            std::cmp::Ordering::Less => bin_path
                .parent()
                .and_then(|p| p.parent())
                .and_then(|p| p.parent())
                .ok_or_else(|| anyhow!("not found 3 parents in {}", bin_path.display()))?,
            std::cmp::Ordering::Equal => {
                bail!("same version: {}", ver);
            }
            std::cmp::Ordering::Greater => {
                bail!("less version: {}", ver);
            }
        };
        println!("found mvn path: {}", mvn_path.display());
        self.uninstall().await?;
        self.install(Some(&ver.to_string())).await?;
        Ok(())
    }

    async fn uninstall(&self) -> Result<()> {
        let bin_link_path = which("mvn").map_err(|e| anyhow!("not found maven path: {}", e))?;
        if let Some(exe_path) = self.base_dir.executable_dir().map(|p| p.join("mvn")) {
            if exe_path != bin_link_path {
                bail!(
                    "inconsistent bin path: {}, original path: {}",
                    bin_link_path.display(),
                    exe_path.display()
                );
            }
        }

        let bin_path = bin_link_path
            .read_link()
            .map_err(|e| anyhow!("{} is not a link: {}", bin_link_path.display(), e))?;

        let ver = find_mvn_version(&bin_path)?;

        let bin_data_paths = glob(&format!(
            "{}/*{}*/**/mvn",
            self.project_dirs.data_dir().display(),
            ver,
        ))?
        .collect::<Result<Vec<_>, _>>()
        .map_err::<Error, _>(Into::into)?;
        debug!("found bin exe paths {:?}", bin_data_paths);
        if bin_data_paths.is_empty() {
            bail!(
                "not found mvn bin in data dir: {}",
                self.project_dirs.data_dir().display()
            );
        } else if bin_data_paths.len() >= 2 {
            bail!("found multiple bin paths: {:?}", bin_data_paths);
        }

        // remove mvn home
        let installed_path = &bin_data_paths[0];
        let mvn_home = installed_path
            .parent()
            .and_then(|p| p.parent())
            .ok_or_else(|| anyhow!("not found 2 parents dir for {}", installed_path.display()))?;
        println!("removing a mvn home {}", mvn_home.display());
        remove_dir_all(mvn_home)?;

        // remove link
        println!("removing a mvn link {}", bin_link_path.display());
        remove_file(&bin_link_path)?;
        Ok(())
    }

    async fn install(&self, version: Option<&str>) -> Result<()> {
        if let Ok(p) = which("mvn") {
            bail!(
                "found installed version {} in {}",
                find_mvn_version(&p)?,
                p.display()
            );
        }

        let install_path = self.project_dirs.data_dir();
        // check path
        if !install_path.exists() {
            info!("creating dir {} for installation", install_path.display());
            afs::create_dir_all(install_path).await?;
        } else if !install_path.is_dir() {
            bail!("{} is not a dir", install_path.display());
        }

        // match mvn version
        let mvn_version = if let Some(ver_pat) = version {
            self.manager.match_version(ver_pat).await?
        } else {
            self.manager.latest_version().await?
        };
        // download
        let down_path = self.manager.download(&mvn_version).await?;
        // extract to path
        extract(down_path.as_path(), install_path)?;

        // link to $PATH
        let exe_path = glob(&format!("{}/**/mvn", install_path.display()))
            .map_err::<Error, _>(Into::into)?
            .flatten()
            .next()
            .ok_or_else(|| anyhow!("not found mvn bin in {}", install_path.display()))?
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
                    println!("installation successful. just type: mvn --version");
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

    async fn list(&self, limit: usize) -> Result<()> {
        let vers = self.manager.versions().await?;
        let limit = if vers.len() < limit {
            vers.len()
        } else {
            limit
        };
        println!("fetching info of {} versions:", limit);
        let bins = self.manager.get_multi_bins(&vers[..limit]).await?;

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
        let p = which("mvn")?;

        debug!("found mvn path: {}", p.display());
        let cur_ver = find_mvn_version(p.clone())?;
        println!(
            "found installed maven version: {}, path: {}",
            cur_ver,
            p.display()
        );
        let latest_ver = self.manager.latest_version().await?;

        let (cur_date, latest_date) = try_join!(
            self.manager.site.fetch_bins(cur_ver.clone()),
            self.manager.site.fetch_bins(latest_ver.clone())
        )
        .map(|(cur_bins, latest_bins)| {
            (
                cur_bins[0].last_modified().date().to_string(),
                latest_bins[0].last_modified().date().to_string(),
            )
        })?;

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
        Ok(())
    }
}

struct Manager {
    site: Site,
    cache_dir: PathBuf,
    versions: Arc<Mutex<Vec<Version>>>,
}

impl Manager {
    pub fn new(site: Site) -> Result<Self> {
        let project_dirs = ProjectDirs::from("xyz", "navyd", CRATE_NAME)
            .ok_or_else(|| anyhow!("project dir error"))?;
        let cache_dir = project_dirs.cache_dir().to_path_buf();
        std::fs::create_dir_all(&cache_dir)?;
        Ok(Self {
            versions: Arc::new(Mutex::new(vec![])),
            site,
            cache_dir,
        })
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

    async fn download(&self, ver: &Version) -> Result<PathBuf> {
        let bins = self.site.fetch_bins(ver.clone()).await?;
        let select_bin = self.choose_bin(&bins)?;

        let down_path = self.cache_dir.join(select_bin.filename());
        if down_path.is_file() && match_digests(down_path.as_path(), select_bin) {
            // cache
            println!("using cached file: {}", down_path.display());
        } else {
            println!("downloading {} of version: {}", select_bin.filename(), ver);
            select_bin.download(down_path.as_path()).await?;
        }
        Ok(down_path)
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
            .and_then(|vers| vers.first().cloned().ok_or_else(|| anyhow!("")))
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
    // use once_cell::sync::Lazy;
    // use tempfile::{tempdir, TempDir};

    // use super::*;

    // static TEMP_CACHE_DIR: Lazy<TempDir> = Lazy::new(|| tempdir().unwrap());

    // fn new(opt: &Opt) -> Result<Program> {
    //     let base_dir = BaseDirs::new().ok_or_else(|| anyhow!("not found base dir"))?;
    //     let cache_dir = TEMP_CACHE_DIR.path().join(CRATE_NAME);
    //     std::fs::create_dir_all(&cache_dir)?;
    //     Ok(Program {
    //         versions: Arc::new(Mutex::new(vec![])),
    //         site: Site::new(opt.mirror.clone()).expect("new site error"),
    //         opt: opt.clone(),
    //         cache_dir,
    //         base_dir,
    //     })
    // }

    // #[tokio::test]
    // async fn test_match_version() -> Result<()> {
    //     let opt = Opt::from_iter([] as [&str; 0]);
    //     let p = new(&opt)?;
    //     let latest_ver = p.latest_version().await?;

    //     let res = p.match_version(&latest_ver.to_string()).await?;
    //     assert_eq!(res, latest_ver);

    //     let res = p.match_version(">= 3").await?;
    //     assert_eq!(res, latest_ver);

    //     let ver = "3.8.3";
    //     let res = p.match_version(&format!("<= {}", ver)).await?;
    //     assert_eq!(res, ver.parse()?);

    //     let ver = "3.8";
    //     let res = p.match_version(&format!("< {}", ver)).await?;
    //     assert_eq!(res, "3.6.3".parse()?);
    //     Ok(())
    // }
}
