use std::path::Path;

use anyhow::{anyhow, bail, Result};
use cmd_lib::run_fun;
use log::{debug, error, trace};
use regex::Regex;
use semver::Version;
use url::Url;
use which::which;

use crate::site::BinFile;

pub fn match_digests(path: impl AsRef<Path>, bin: &BinFile) -> bool {
    let data = path.as_ref().metadata().unwrap();
    // todo: digest
    data.len() == *bin.size() as u64
}

pub fn extract<P: AsRef<Path>>(from: P, to: P) -> Result<()> {
    let (from, to) = (from.as_ref(), to.as_ref());
    if !from.is_file() {
        bail!("{} is not a file", from.display());
    }
    if let Ok(p) = which("tar") {
        debug!("try using tar to extract {}", from.display());
        let to_str = to.to_str().unwrap();
        let out = run_fun!($p xvf $from --directory=$to_str).map_err(|e| {
            error!("failed to extract by tar: {}", e);
            e
        })?;
        trace!("tar output: {}", out);
        return Ok(());
    }
    bail!("failed to extract file {}: unsupported", from.display())
}

/// 从url中查找文件名
pub fn get_filename(url: impl AsRef<str>) -> Result<String> {
    let url = url.as_ref().parse::<Url>()?;
    Path::new(url.path())
        .file_name()
        .and_then(|e| e.to_str().map(ToString::to_string))
        .ok_or_else(|| anyhow!("not found filename for {}", url))
}

pub fn find_mvn_version(path: impl AsRef<Path>) -> Result<Version> {
    // let cmd = format!("{} --version", path.as_ref().to_str().expect("to str error"));
    // trace!("running command: {}", cmd);
    let path_str = path.as_ref().to_str().expect("to str error");
    let out = run_fun! {$path_str --version}?;
    parse_mvn_version(&out)?.parse().map_err(Into::into)
}

pub fn find_java_version(path: impl AsRef<Path>) -> Result<String> {
    let path_str = path.as_ref().to_str().expect("to str error");
    // java -version output to stderr
    let out = run_fun! {2>&1 $path_str -version}?;
    parse_java_version(&out)
}

fn parse_mvn_version(s: &str) -> Result<String> {
    let re = Regex::new(r"Apache Maven\s*((\d+\.?)*)")?;
    re.captures(s)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_string())
        .ok_or_else(|| {
            error!("failed to parse java version by regex `{}`: {}", re, s);
            anyhow!("failed to parse mvn version")
        })
}

fn parse_java_version(s: &str) -> Result<String> {
    let re = Regex::new(r#"version "(.+)""#)?;
    re.captures(s)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_string())
        .ok_or_else(|| {
            error!("failed to parse java version by regex `{}`: {}", re, s);
            anyhow!("not found java version")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_mvn_version() -> Result<()> {
        let out = r#"Apache Maven 3.8.3 (ff8e977a158738155dc465c6a97ffaf31982d739)
Maven home: /home/navyd/.zinit/snippets/apache-maven-3.8.3
Java version: 17, vendor: Private Build, runtime: /usr/lib/jvm/java-17-openjdk-amd64
Default locale: en, platform encoding: UTF-8
OS name: "linux", version: "5.10.60.1-microsoft-standard-wsl2", arch: "amd64", family: "unix""#;
        assert_eq!(parse_mvn_version(out)?, "3.8.3".to_string());
        Ok(())
    }

    #[test]
    fn test_parse_java_version() -> Result<()> {
        let ver_17 = r#"openjdk version "17" 2021-09-14
OpenJDK Runtime Environment (build 17+35-Ubuntu-120.04)
OpenJDK 64-Bit Server VM (build 17+35-Ubuntu-120.04, mixed mode, sharing)"#;
        assert_eq!(parse_java_version(ver_17)?, "17");

        let ver_8 = r#"openjdk version "1.8.0_312"
OpenJDK Runtime Environment (build 1.8.0_312-b07)
OpenJDK 64-Bit Server VM (build 25.312-b07, mixed mode)"#;
        assert_eq!(parse_java_version(ver_8)?, "1.8.0_312");

        // localhost
        // let ver = find_java_version(which::which("java")?)?;
        // assert_eq!(ver, "17");
        Ok(())
    }
}
