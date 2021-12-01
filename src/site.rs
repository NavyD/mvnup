use anyhow::{anyhow, bail, Error, Result};
use chrono::{Date, DateTime, Local};
use futures_util::StreamExt;
use log::{debug, error, trace};
use scraper::{Html, Selector};
use std::{fmt::Display, path::Path, str::FromStr};
use tempfile::tempfile;
use tokio::fs as afs;
use url::Url;

/// [What do the numbers in a version typically represent (i.e. v1.9.0.1)?](https://stackoverflow.com/a/24941508/8566831)
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

pub struct PublishedInfo {
    release_date: Date<Local>,
    version: String,
    required_java_version: String,
    notes: String,
}

#[derive(Debug)]
struct MvnBinFile {
    filename: String,
    last_modified: DateTime<Local>,
    size: usize,
    sha512: Option<String>,
    md5: Option<String>,
    sha1: Option<String>,
}

impl MvnBinFile {
    fn has_digests(&self) -> bool {
        self.sha1
            .as_ref()
            .or_else(|| self.md5.as_ref())
            .or_else(|| self.sha512.as_ref())
            .is_some()
    }
}

struct Site {
    mirror: Url,
}

impl Site {
    pub fn new<U>(mirror: U) -> Result<Self>
    where
        U: TryInto<Url> + Display,
        U::Error: Into<Error>,
    {
        let mirror = mirror.try_into().map_err(Into::into)?;
        Ok(Self { mirror })
    }

    /// 获取版本信息
    pub async fn fetch_versions(&self) -> Result<Vec<Version>> {
        let url = format!("{}/maven/maven-3/", self.mirror);
        debug!("fetching versions from {}", url);
        parse_versions(&reqwest::get(&url).await?.text().await?)
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

    /// 获取binaries中的文件信息
    pub async fn fetch_bins(&self, ver: &Version) -> Result<Vec<MvnBinFile>> {
        let url = self
            .mirror
            .join(&format!("maven/maven-3/{}/binaries/", ver))?;
        debug!("fetching binaries for {}", url);
        let body = reqwest::get(url.clone()).await?.text().await?;
        let names = parse_bin_names(&body)?;

        trace!("finding digests in files: {:?}", names);

        let fetch_cxt = |url: Url| async move {
            debug!("fetching content from {}", url);
            let resp = reqwest::get(url.clone()).await?;
            let mut stream = resp.bytes_stream();
            let mut cxt = String::new();
            while let Some(chunk) = stream.next().await {
                let s = chunk?.into_iter().collect::<Vec<_>>();
                let s = std::str::from_utf8(&s)?;
                cxt.push_str(s);
            }
            trace!("found content: {} for {}", cxt, url);
            Ok::<_, Error>(cxt)
        };

        let mut files = vec![];
        for name in names {
            let mut file = fetch_bin_metadata(url.join(&name)?).await?;

            let digest_filename = format!("{}.md5", name);
            if body.contains(&digest_filename) {
                file.md5
                    .replace(fetch_cxt(url.join(&digest_filename)?).await?);
            }

            let digest_filename = format!("{}.sha512", name);
            if body.contains(&digest_filename) {
                file.sha512
                    .replace(fetch_cxt(url.join(&digest_filename)?).await?);
            }

            let digest_filename = format!("{}.sha1", name);
            if body.contains(&digest_filename) {
                file.sha1
                    .replace(fetch_cxt(url.join(&digest_filename)?).await?);
            }

            if !file.has_digests() {
                error!("failed to found digest: {:?} for {}", file, name);
                bail!("not found digest for {}", name)
            }

            trace!("found a mvn file: {:?}", file);
            files.push(file);
        }
        Ok(files)
    }
}

/// 对url使用head请求获取binaries文件元数据，不填充其它可选字段。
/// 如：https://archive.apache.org/dist/maven/maven-3/3.8.4/binaries/apache-maven-3.8.4-bin.tar.gz
async fn fetch_bin_metadata(url: Url) -> Result<MvnBinFile> {
    let filename = Path::new(url.path())
        .file_name()
        .and_then(|e| e.to_str().map(ToString::to_string))
        .ok_or_else(|| anyhow!("not found filename"))?;

    debug!("fetching bin {} metadata for {}", filename, url);
    let resp = reqwest::Client::builder()
        .build()?
        .head(url.as_str())
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!(
            "failed to response status {} for {}",
            resp.status(),
            url.as_str()
        );
    }
    let headers = resp.headers();
    trace!("parsing mvn files info in headers: {:?}", headers);

    let name = "Content-Length";
    let size = if let Some(val) = headers.get(name) {
        trace!("parsing header {}={:?}", name, val);
        val.to_str()?.parse::<usize>()?
    } else {
        bail!("not found header: {}", name)
    };

    let name = "Last-Modified";
    let last_modified = if let Some(val) = headers.get(name) {
        trace!("parsing header {}={:?}", name, val);
        DateTime::parse_from_rfc2822(val.to_str()?)?.with_timezone(&Local)
    } else {
        bail!("not found header: {}", name)
    };
    Ok(MvnBinFile {
        filename,
        last_modified,
        size,
        md5: None,
        sha1: None,
        sha512: None,
    })
}

/// 解析页面`https://archive.apache.org/dist/maven/maven-3/3.8.4/binaries/`中的版本文件名
fn parse_bin_names(content: &str) -> Result<Vec<String>> {
    trace!("parsing bin names in content: {}", content);
    let html = Html::parse_document(content);
    let link_selector = Selector::parse("img[alt='  ']+a").map_err(|e| {
        anyhow!(
            "failed to parsing. kind: {:?}, location: {:?}",
            e.kind,
            e.location
        )
    })?;
    let names = html
        .select(&link_selector)
        .map(|e| e.inner_html().trim().to_string())
        .collect::<Vec<_>>();
    Ok(names)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_versions() -> Result<()> {
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
        assert_eq!(
            versions.first().map(ToString::to_string),
            Some("3.0.4".to_string())
        );
        assert_eq!(
            versions.last().map(ToString::to_string),
            Some("3.8.4".to_string())
        );
        Ok(())
    }
}
