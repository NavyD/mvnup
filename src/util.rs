use std::path::Path;

use anyhow::{anyhow, Result};
use cmd_lib::run_fun;
use log::trace;
use regex::Regex;
use semver::Version;
use url::Url;

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

fn parse_mvn_version(s: &str) -> Result<String> {
    trace!("parsing mvn version: {}", s);
    let s = s.trim();
    let re = Regex::new(r"Apache Maven\s*((\d+\.?)*)")?;
    let caps = re
        .captures(s)
        .ok_or_else(|| anyhow!("not found regex: {}", s))?;
    caps.get(1)
        .map(|e| e.as_str().to_string())
        .ok_or_else(|| anyhow!("failed to parse mvn version in caps: {:?}", caps))
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
}
