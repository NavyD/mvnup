use std::{
    fmt::Display,
    io::Write,
    path::{Path, PathBuf},
    str::FromStr,
};

use cmd_lib::{run_cmd, run_fun};
use regex::Regex;
use tokio::fs as afs;

use anyhow::{anyhow, bail, Error, Result};
use chrono::{Date, DateTime, Local};
use futures_util::StreamExt;
use log::{debug, error, trace};
use scraper::{Html, Selector};
use structopt::StructOpt;
use tempfile::tempfile;
use tokio::io::AsyncWriteExt;
use url::{ParseError, Url};
use which::which;

#[tokio::main]
async fn main() -> Result<()> {
    let opt = Opt::from_args();
    opt.run().await
    // init_log(3)?;
    // if let Some(a) = opt.commands {
    // } else {
    //     let mvn = MvnUp {
    //         mvn: MavenInfo::new(opt.mirror.clone()).await?,
    //         opt,
    //     };
    //     mvn.check()?;
    // }
}

#[derive(Debug, StructOpt)]
pub struct Opt {
    #[structopt(long, short, default_value = "https://archive.apache.org/dist/")]
    mirror: Url,

    #[structopt(long, short, parse(from_occurrences))]
    verbose: u8,

    #[structopt(subcommand)]
    commands: Option<Commands>,
}

impl Opt {
    pub async fn run(&self) -> Result<()> {
        self.init_log()?;

        todo!()
    }

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
    List,
}

#[derive(Debug, StructOpt)]
struct InstallArgs {
    #[structopt(long, short, default_value = "")]
    path: PathBuf,

    #[structopt(long, short, default_value = "")]
    version: String,
}

struct MvnUp {
    mvn: MavenInfo,
}

impl MvnUp {
    fn install(&self, args: &InstallArgs) -> Result<()> {
        todo!()
    }

    fn list(&self) -> Result<()> {
        todo!()
    }

    fn check(&self) -> Result<()> {
        // install
        match which("mvn") {
            Ok(_) => {
                let mvn_out = run_fun! {mvn --version}?;
                let cur = parse_mvn_version(&mvn_out)?;
                println!("{}", mvn_out);
                let latest = self.mvn.latest_version();
                use std::cmp::Ordering::*;
                match cur.cmp(&latest.to_string()) {
                    Equal => {
                        println!("up to date: {}", cur);
                    }
                    Less => {
                        println!("update available: {} -> {}", cur, latest);
                    }
                    Greater => {
                        bail!("failure version. cur: {}. latest: {}", cur, latest);
                    }
                }
            }
            Err(e) => {
                bail!(
                    "not found mvn: {}\nmvn all versions:{:?}",
                    e,
                    self.mvn.versions()
                );
            }
        }
        Ok(())
    }
}

fn parse_mvn_version(s: &str) -> Result<String> {
    let mvn_out = s.trim();
    let re = Regex::new(r"Apache Maven\s*((\d+\.?)*)")?;
    let caps = re
        .captures(mvn_out)
        .ok_or_else(|| anyhow!("not found regex: {}", mvn_out))?;
    caps.get(1)
        .map(|e| e.as_str().to_string())
        .ok_or_else(|| anyhow!("failed to parse mvn version in caps: {:?}", caps))
}

/// 从html中解析出版本信息
fn parse_versions(content: &str) -> Result<Vec<String>> {
    trace!("parsing content: {}", content);
    let html = Html::parse_document(content);
    let link_selector = Selector::parse("img[alt='[DIR]']+a").map_err(|e| {
        anyhow!(
            "failed to parsing. kind: {:?}, location: {:?}",
            e.kind,
            e.location
        )
    })?;
    let versions = html
        .select(&link_selector)
        .map(|e| e.inner_html().trim().replace("/", ""))
        .collect::<Vec<_>>();
    Ok(versions)
}

async fn fetch_versions(mirror: &str) -> Result<Vec<String>> {
    let url = format!("{}/maven/maven-3/", mirror);
    debug!("fetching versions from {}", url);
    let body = reqwest::get(&url).await?.text().await?;
    parse_versions(&body)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    major: String,
    minor: String,
    patch: String,
}

impl Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl FromStr for Version {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        trace!("parsing version with {}", value);
        let v = value.split('.').collect::<Vec<_>>();
        debug!("parsed version: {:?}", v);
        if v.len() != 3 {
            bail!("invalid version format: {}, length: {}", value, v.len());
        }
        Ok(Version {
            major: v[0].to_string(),
            minor: v[1].to_string(),
            patch: v[2].to_string(),
        })
    }
}

struct MavenInfo {
    versions: Vec<String>,
    mirror: Url,
}

impl MavenInfo {
    /// [Impl TryInto as an argument in a function complains about the Error conversion](https://users.rust-lang.org/t/impl-tryinto-as-an-argument-in-a-function-complains-about-the-error-conversion/34004/2)
    pub async fn new<U>(mirror: U) -> Result<Self>
    where
        U: TryInto<Url> + Display,
        U::Error: Into<Error>,
    {
        let mirror: Url = mirror.try_into().map_err(Into::into)?;
        let mut versions = fetch_versions(mirror.as_str()).await?;
        if versions.is_empty() {
            bail!("empty versions for mirror: {}", mirror)
        }
        versions.sort_unstable_by(|a, b| b.cmp(a));
        Ok(MavenInfo { versions, mirror })
    }

    pub fn versions(&self) -> &[String] {
        &self.versions
    }

    pub fn latest_version(&self) -> &str {
        self.versions.first().unwrap()
    }

    pub fn zip_bin_url(&self, ver: &str) -> Url {
        self.mirror
            .join(&format!(
                "maven-3/{}/binaries/apache-maven-{}-bin.zip",
                ver, ver
            ))
            .unwrap()
    }

    pub async fn download(&self, ver: &str) -> Result<afs::File> {
        let url = self.zip_bin_url(ver);
        let resp = reqwest::get(url).await?;
        let mut stream = resp.bytes_stream();
        let mut tmpfile = afs::File::from_std(tempfile()?);
        while let Some(chunk) = stream.next().await {
            let mut chunk = chunk?;
            tmpfile.write_all_buf(&mut chunk).await?;
        }
        Ok(tmpfile)
    }
}

mod web {
    use super::*;

    pub struct Published {
        release_date: Date<Local>,
        version: String,
        required_java_version: String,
        notes: String,
    }

    pub async fn fetch_versions(mirror: &str) -> Result<Vec<Version>> {
        let url = format!("{}/maven/maven-3/", mirror);
        debug!("fetching versions from {}", url);
        let body = reqwest::get(&url).await?.text().await?;
        parse_versions(&body)
    }

    /// 从html中解析出版本信息
    fn parse_versions(content: &str) -> Result<Vec<Version>> {
        trace!("parsing content: {}", content);
        let html = Html::parse_document(content);
        let link_selector = Selector::parse("img[alt='[DIR]']+a").map_err(|e| {
            anyhow!(
                "failed to parsing. kind: {:?}, location: {:?}",
                e.kind,
                e.location
            )
        })?;
        let versions = html
            .select(&link_selector)
            .map(|e| e.inner_html().trim().replace("/", ""))
            .flat_map(|e| e.parse::<Version>())
            .collect::<Vec<_>>();
        Ok(versions)
    }

    pub async fn fetch_releases() -> Result<Vec<Published>> {
        let url = "https://maven.apache.org/docs/history.html";
        debug!("fetching releases from {}", url);
        let body = reqwest::get(url).await?.text().await?;
        parse_history(&body)
    }

    // fn css_err<T>(e: scraper) -> Result<T> {
    //         let a = anyhow!(
    //             "failed to parsing. kind: {:?}, location: {:?}",
    //             e.kind,
    //             e.location
    //         );
    //         todo!()
    // }

    fn parse_history(content: &str) -> Result<Vec<Published>> {
        trace!("parsing history: {}", content);

        let html = Html::parse_document(content);
        // let h3 = Selector::parse("#maven-3-1").map_err(f)?;
        // let h3 = html.select(&h3).next();
        // let h3.parent().unwrap();

        todo!()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
    
        #[test]
        fn test_name() {
            
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_select() -> Result<()> {
        let body = r#"
<!DOCTYPE HTML PUBLIC "-//W3C//DTD HTML 3.2 Final//EN">
<html>
 <head>
  <title>Index of /dist/maven/maven-3</title>
 </head>
 <body>
<h1>Index of /dist/maven/maven-3</h1>
<pre><img src="/icons/blank.gif" alt="Icon "> <a href="?C=N;O=D">Name</a>                    <a href="?C=M;O=A">Last modified</a>      <a href="?C=S;O=A">Size</a>  <a href="?C=D;O=A">Description</a><hr><img src="/icons/back.gif" alt="[PARENTDIR]"> <a href="/dist/maven/">Parent Directory</a>                             -
<img src="/icons/folder.gif" alt="[DIR]"> <a href="3.0.4/">3.0.4/</a>                  2012-09-11 09:37    -
<img src="/icons/folder.gif" alt="[DIR]"> <a href="3.0.5/">3.0.5/</a>                  2020-07-03 04:01    -
<img src="/icons/folder.gif" alt="[DIR]"> <a href="3.1.0-alpha-1/">3.1.0-alpha-1/</a>          2013-06-07 06:32    -
<img src="/icons/folder.gif" alt="[DIR]"> <a href="3.1.0/">3.1.0/</a>                  2013-07-14 13:03    -
<img src="/icons/folder.gif" alt="[DIR]"> <a href="3.1.1/">3.1.1/</a>                  2020-07-03 04:01    -
<img src="/icons/folder.gif" alt="[DIR]"> <a href="3.2.1/">3.2.1/</a>                  2014-03-10 11:08    -
<img src="/icons/folder.gif" alt="[DIR]"> <a href="3.2.2/">3.2.2/</a>                  2014-06-26 00:11    -
<img src="/icons/folder.gif" alt="[DIR]"> <a href="3.2.3/">3.2.3/</a>                  2014-08-15 17:30    -
<img src="/icons/folder.gif" alt="[DIR]"> <a href="3.2.5/">3.2.5/</a>                  2020-07-03 04:01    -
<img src="/icons/folder.gif" alt="[DIR]"> <a href="3.3.1/">3.3.1/</a>                  2015-03-17 17:28    -
<img src="/icons/folder.gif" alt="[DIR]"> <a href="3.3.3/">3.3.3/</a>                  2015-04-28 15:12    -
<img src="/icons/folder.gif" alt="[DIR]"> <a href="3.3.9/">3.3.9/</a>                  2020-07-03 04:01    -
<img src="/icons/folder.gif" alt="[DIR]"> <a href="3.5.0-alpha-1/">3.5.0-alpha-1/</a>          2017-02-28 22:25    -
<img src="/icons/folder.gif" alt="[DIR]"> <a href="3.5.0-beta-1/">3.5.0-beta-1/</a>           2017-03-24 10:48    -
<img src="/icons/folder.gif" alt="[DIR]"> <a href="3.5.0/">3.5.0/</a>                  2017-10-04 10:47    -
<img src="/icons/folder.gif" alt="[DIR]"> <a href="3.5.2/">3.5.2/</a>                  2018-05-04 11:19    -
<img src="/icons/folder.gif" alt="[DIR]"> <a href="3.5.3/">3.5.3/</a>                  2018-05-04 11:19    -
<img src="/icons/folder.gif" alt="[DIR]"> <a href="3.5.4/">3.5.4/</a>                  2020-07-03 04:01    -
<img src="/icons/folder.gif" alt="[DIR]"> <a href="3.6.0/">3.6.0/</a>                  2018-10-31 16:43    -
<img src="/icons/folder.gif" alt="[DIR]"> <a href="3.6.1/">3.6.1/</a>                  2019-09-03 16:54    -
<img src="/icons/folder.gif" alt="[DIR]"> <a href="3.6.2/">3.6.2/</a>                  2019-09-03 20:13    -
<img src="/icons/folder.gif" alt="[DIR]"> <a href="3.6.3/">3.6.3/</a>                  2020-07-03 04:01    -
<img src="/icons/folder.gif" alt="[DIR]"> <a href="3.8.1/">3.8.1/</a>                  2021-04-04 12:24    -
<img src="/icons/folder.gif" alt="[DIR]"> <a href="3.8.2/">3.8.2/</a>                  2021-08-13 19:53    -
<img src="/icons/folder.gif" alt="[DIR]"> <a href="3.8.3/">3.8.3/</a>                  2021-10-03 16:34    -
<img src="/icons/folder.gif" alt="[DIR]"> <a href="3.8.4/">3.8.4/</a>                  2021-11-20 14:43    -
<hr></pre>
</body></html>
        "#;
        let versions = parse_versions(body)?;
        assert_eq!(versions.len(), 26);
        assert_eq!(versions.first(), Some(&"3.0.4".to_string()));
        assert_eq!(versions.last(), Some(&"3.8.4".to_string()));
        Ok(())
    }

    #[test]
    fn test_mvn_parse() -> Result<()> {
        let out = r#"Apache Maven 3.8.3 (ff8e977a158738155dc465c6a97ffaf31982d739)
Maven home: /home/navyd/.zinit/snippets/apache-maven-3.8.3
Java version: 17, vendor: Private Build, runtime: /usr/lib/jvm/java-17-openjdk-amd64
Default locale: en, platform encoding: UTF-8
OS name: "linux", version: "5.10.60.1-microsoft-standard-wsl2", arch: "amd64", family: "unix""#;
        assert_eq!(parse_mvn_version(out)?, "3.8.3".to_string());
        Ok(())
    }
}
