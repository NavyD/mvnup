use crate::util::get_filename;
use anyhow::{anyhow, bail, Error, Result};
use chrono::{DateTime, Local};
use futures_util::{future::join_all, join, try_join, StreamExt, TryFutureExt};
use getset::Getters;
use log::{debug, error, info, log_enabled, trace, warn};
use mime::Mime;
use once_cell::sync::Lazy;
use reqwest::Client;
use scraper::{Html, Selector};
use semver::Version;
use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
    io::{BufReader, Cursor, Read},
    path::Path,
    time::Duration,
};
use strum::{AsRefStr, Display, EnumString, EnumVariantNames, VariantNames};
use tokio::{fs as afs, io::AsyncWriteExt};
use url::Url;

pub static HTTP_CLIENT: Lazy<Client> = Lazy::new(|| {
    Client::builder()
        .timeout(Duration::from_secs(4))
        .build()
        .expect("build client failed")
});

// macro_rules! field_names {
//     (struct $name:ident {
//         $($field_name:ident: $field_type:ty,)*
//     }) => {
//         #[derive(Debug, PartialEq, Eq, Clone, Getters, Default)]
//         pub struct $name {
//             $($field_name: $field_type,)*
//         }

//         impl $name {
//             // This is purely an example—not a good one.
//             fn get_field_names() -> Vec<&'static str> {
//                 vec![$(stringify!($field_name)),*]
//             }

//             fn from_entries(v: impl std::iter::IntoIterator<Item = (String, Option<Option<String>>)>) -> Self {
//                 let v = v.into_iter().collect::<::std::collections::HashMap<_, _>>();
//                 Self {$(
//                     $field_name: v["$field_name"].clone(),
//                 )*}
//             }
//         }
//     }
// }
macro_rules! field_names {
    (
        $(#[$m:meta])?
        $aa:vis enum $name:ident {
        $($field_name:ident($field_type:ty))*

    }) => {
        // #[derive(Debug, PartialEq, Eq, Clone, Getters, Default)]
        // pub struct $name {
        //     $($field_name: $field_type,)*
        // }
        $aa enum $name {
            $($field_name($field_type))*
        }

        impl $name {
            pub fn new(variant_name: &str, cxt: &str) -> Self {
                todo!()
            }
        }
    }
}

// field_names! {
//     struct Digest {
//         sha512: Option<Option<String>>,
//         md5: Option<Option<String>>,
//         sha1: Option<Option<String>>,
//     }
// }

#[derive(Debug, Clone, PartialEq, Eq, EnumVariantNames, EnumString, AsRefStr)]
pub enum Digest {
    Sha512(String),
    Md5(String),
    Sha1(String),
}

#[derive(Debug, PartialEq, Eq, Clone, Getters)]
#[getset(get = "pub")]
pub struct BinFile {
    url: Url,
    filename: String,
    last_modified: DateTime<Local>,
    size: usize,
    mime: Mime,
    digest: Option<Digest>,
}

impl BinFile {
    pub async fn download(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        trace!("starting download to {} for {}", path.display(), self.url());
        let mut file = afs::File::create(path).await?;
        let resp = reqwest::get(self.url.clone()).await?;
        debug!(
            "downloading file content length: {:?}, size: {}",
            resp.content_length(),
            self.size
        );
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let mut chunk = chunk?;
            file.write_all_buf(&mut chunk).await?;
        }
        if log_enabled!(log::Level::Info) {
            info!(
                "download completed. file {} size: {}",
                path.display(),
                file.metadata().await?.len()
            );
        }
        // self.digest.map(|digest| digest.check(s))
        if let Some(d) = self.digest() {
            // todo!()
            return Ok(());
        } else {
            warn!("{} digests not checked", path.display());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Getters)]
pub struct Site {
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

    /// 获取binaries中的文件信息
    pub async fn fetch_bins(&self, ver: Version) -> Result<Vec<BinFile>> {
        // find binaries info
        // let ver = ver.as_ref().parse::<Version>()?;
        let url = self
            .mirror
            .join(&format!("maven/maven-3/{}/binaries/", ver))?;

        // concurrent
        debug!("fetching {} binaries for {}", ver, url);
        let content = HTTP_CLIENT.get(url.clone()).send().await?.text().await?;
        let tasks = parse_bin_names(&content)?
            .into_iter()
            .map(|name| url.join(&name).map_err::<Error, _>(Into::into))
            .map(|bin_url| {
                bin_url.map(|url| {
                    let content = content.clone();
                    async move {
                        trace!("fetching metadata and digest for {} in concurrent", url);
                        try_join!(fetch_bin_metadata(&url), fetch_bin_digest(&url, &content)).map(
                            |((filename, mime, size, last_modified), digest)| BinFile {
                                digest,
                                filename,
                                last_modified,
                                mime,
                                size,
                                url,
                            },
                        )
                    }
                })
            })
            .collect::<Result<Vec<_>>>()?;
        join_all(tasks)
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
    }
}

/// 对url使用head请求获取binaries文件元数据
/// 如：https://archive.apache.org/dist/maven/maven-3/3.8.4/binaries/apache-maven-3.8.4-bin.tar.gz
async fn fetch_bin_metadata(url: &Url) -> Result<(String, Mime, usize, DateTime<Local>)> {
    // parse http headers
    let filename = get_filename(&url)?;
    debug!("fetching bin metadata {} for {}", filename, url);
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

    // parse headers
    let headers = resp.headers();
    trace!("parsing mvn files info in headers: {:?}", headers);
    let parse_header = |name| {
        if let Some(val) = headers.get(name) {
            trace!("parsing header {}={:?}", name, val);
            val.to_str().map_err(Into::into)
        } else {
            bail!("not found header: {}", name)
        }
    };

    let mime = parse_header("Content-Type")?.parse::<Mime>()?;
    let size = parse_header("Content-Length")?.parse::<usize>()?;
    let last_modified = parse_header("Last-Modified")
        .and_then(|s| DateTime::parse_from_rfc2822(s).map_err(Into::into))
        .map(|d| d.with_timezone(&Local))?;
    Ok((filename, mime, size, last_modified))
}

async fn fetch_cxt(url: Url) -> Result<String> {
    let dup_url = url.to_string();
    trace!("fetching digest content for {}", url);
    let filename = get_filename(&url)?;
    let resp = reqwest::get(url.as_str()).await?;
    if !resp.status().is_success() {
        trace!(
            "failed to fetch digest for {}. status: {}, headers: {:?}",
            url,
            resp.status(),
            resp.headers()
        );
        bail!(
            "failed to get digest {}. status: {}",
            filename,
            resp.status()
        );
    }
    debug!("found digest for {}", filename);
    resp.text().await.map_err(Into::into).map_err(move |e| {
        info!("failed to fetch digest for {}: {}", dup_url, e);
        e
    })
}

/// 解析页面`https://archive.apache.org/dist/maven/maven-3/3.8.4/binaries/`中的版本文件名
fn parse_bin_names(content: &str) -> Result<Vec<String>> {
    trace!("parsing bin names in content size: {}", content.len());
    let html = Html::parse_document(content);
    let link_selector = Selector::parse("img[alt*='[  ']+a").map_err(|e| {
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
    if names.is_empty() {
        error!("failed to parse bin names empty in content: {}", content);
        bail!("not found bin names");
    }
    Ok(names)
}

async fn fetch_bin_digest(bin_url: &Url, content: &str) -> Result<Option<Digest>> {
    let bin_name = Path::new(bin_url.path())
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("not found filename for {}", bin_url))?;
    let content = content.to_ascii_lowercase();
    for ext_name in Digest::VARIANTS {
        let digest_filename = format!("{}.{}", bin_name, ext_name).to_ascii_lowercase();
        if content.contains(&digest_filename) {
            let digest_url = bin_url.join(&digest_filename)?;
            let cxt = fetch_cxt(digest_url).await?;
            let mut digest = ext_name.parse::<Digest>()?;
            match &mut digest {
                Digest::Md5(s) => {
                    s.push_str(&cxt);
                }
                Digest::Sha1(s) => {
                    s.push_str(&cxt);
                }
                Digest::Sha512(s) => {
                    s.push_str(&cxt);
                }
            }
            return Ok(Some(digest));
        }
    }
    Ok(None)
}

/// 从html中解析出版本信息
fn parse_versions(content: &str) -> Result<Vec<Version>> {
    trace!("parsing versions in content {}", content.len());
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
    use once_cell::sync::Lazy;

    use super::*;

    /// headers:
    ///
    /// ```
    /// HTTP/1.1 200 OK
    /// Date: Thu, 02 Dec 2021 06:57:55 GMT
    /// Server: Apache
    /// Last-Modified: Sun, 14 Nov 2021 13:25:01 GMT
    /// ETag: "8a08a1-5d0bf9e96832e"
    /// Accept-Ranges: bytes
    /// Content-Length: 9046177
    /// Content-Type: application/x-gzip
    /// ```
    static BIN_FILE: Lazy<BinFile> = Lazy::new(|| {
        BinFile {
            url: "https://archive.apache.org/dist/maven/maven-3/3.8.4/binaries/apache-maven-3.8.4-bin.tar.gz".parse::<Url>().unwrap(),
            filename: "apache-maven-3.8.4-bin.tar.gz".to_string(),
            last_modified: DateTime::parse_from_rfc2822("Sun, 14 Nov 2021 13:25:01 GMT").unwrap().with_timezone(&Local),
            size: 9046177,
            digest: Some(Digest::Sha512("a9b2d825eacf2e771ed5d6b0e01398589ac1bfa4171f36154d1b5787879605507802f699da6f7cfc80732a5282fd31b28e4cd6052338cbef0fa1358b48a5e3c8".to_string())),
            mime: "application/x-gzip".parse().unwrap()
        }
    });

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

    static CONTENT: &str = r#"<!DOCTYPE HTML PUBLIC "-//W3C//DTD HTML 3.2 Final//EN">
<html>
 <head>
  <title>Index of /dist/maven/maven-3/3.8.4/binaries</title>
 </head>
 <body>
<h1>Index of /dist/maven/maven-3/3.8.4/binaries</h1>
<pre><img src="/icons/blank.gif" alt="Icon "> <a href="?C=N;O=D">Name</a>                                 <a href="?C=M;O=A">Last modified</a>      <a href="?C=S;O=A">Size</a>  <a href="?C=D;O=A">Description</a><hr><img src="/icons/back.gif" alt="[PARENTDIR]"> <a href="/dist/maven/maven-3/3.8.4/">Parent Directory</a>                                          -
<img src="/icons/compressed.gif" alt="[   ]"> <a href="apache-maven-3.8.4-bin.tar.gz">apache-maven-3.8.4-bin.tar.gz</a>        2021-11-14 13:25  8.6M
<img src="/icons/text.gif" alt="[TXT]"> <a href="apache-maven-3.8.4-bin.tar.gz.asc">apache-maven-3.8.4-bin.tar.gz.asc</a>    2021-11-14 13:25  484
<img src="/icons/text.gif" alt="[TXT]"> <a href="apache-maven-3.8.4-bin.tar.gz.sha512">apache-maven-3.8.4-bin.tar.gz.sha512</a> 2021-11-14 13:25  128
<img src="/icons/compressed.gif" alt="[   ]"> <a href="apache-maven-3.8.4-bin.zip">apache-maven-3.8.4-bin.zip</a>           2021-11-14 13:25  8.7M
<img src="/icons/text.gif" alt="[TXT]"> <a href="apache-maven-3.8.4-bin.zip.asc">apache-maven-3.8.4-bin.zip.asc</a>       2021-11-14 13:25  484
<img src="/icons/text.gif" alt="[TXT]"> <a href="apache-maven-3.8.4-bin.zip.sha512">apache-maven-3.8.4-bin.zip.sha512</a>    2021-11-14 13:25  128
<hr></pre>
</body></html>"#;

    #[test]
    fn test_parse_bin_names() -> Result<()> {
        let names = parse_bin_names(CONTENT)?;
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"apache-maven-3.8.4-bin.tar.gz".to_string()));
        assert!(names.contains(&"apache-maven-3.8.4-bin.zip".to_string()));
        assert!(!names.contains(&"apache-maven-3.8.4-bin.zip.sha512".to_string()));
        Ok(())
    }

    #[tokio::test]
    async fn test_fetch_bin_metadata() -> Result<()> {
        let bin = BIN_FILE.clone();
        let res = fetch_bin_metadata(bin.url()).await?;

        assert_eq!(res.0, bin.filename);
        assert_eq!(res.1, bin.mime);
        assert_eq!(res.2, bin.size);
        assert_eq!(res.3, bin.last_modified);
        Ok(())
    }

    #[tokio::test]
    async fn test_fetch_bin_digest() -> Result<()> {
        let bin = BIN_FILE.clone();
        let res = fetch_bin_digest(bin.url(), CONTENT).await?;
        assert_eq!(res, bin.digest);
        Ok(())
    }

    #[cfg(test)]
    mod binfile_tests {
        use super::*;

        // #[tokio::test]
        // async fn test_new() -> Result<()> {
        //     let bin = BIN_FILE.clone();
        //     let res = BinFile::new(bin.url.as_ref()).await?;
        //     assert_eq!(bin, res);
        //     Ok(())
        // }
    }

    static ARCHIVE_SITE: Lazy<Site> =
        Lazy::new(|| Site::new("https://archive.apache.org/dist/").unwrap());

    #[cfg(test)]
    mod site_tests {
        use super::*;
        use tokio::test;

        #[test]
        async fn test_fetch_versions() -> Result<()> {
            let site = ARCHIVE_SITE.clone();
            let versions = site.fetch_versions().await?;
            assert!(versions.len() >= 26);
            assert!(versions.contains(&"3.8.4".parse()?));
            Ok(())
        }

        #[test]
        async fn test_fetch_bins() -> Result<()> {
            let site = ARCHIVE_SITE.clone();
            let ver = "3.8.4";
            let bins = site.fetch_bins(ver.parse::<Version>()?).await?;
            assert_eq!(bins.len(), 2);
            bins.iter().for_each(|f| assert!(f.filename.contains(ver)));
            Ok(())
        }
    }
}
