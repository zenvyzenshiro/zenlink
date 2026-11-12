//! Update check (v2.5.1): which GitHub release to offer, and its files.
//!
//! Portable logic only (no network): the app fetches
//! [`RELEASES_URL`], passes the JSON to [`pick`], and downloads the chosen
//! asset only after the user clicks. Every asset must
//! come from this repository's release downloads and carry a SHA-256 digest.

use std::cmp::Ordering;

use serde_json::Value;

pub const REPO: &str = "zenvyzenshiro/zenlink";
pub const RELEASES_URL: &str =
    "https://api.github.com/repos/zenvyzenshiro/zenlink/releases?per_page=30";
pub const RELEASES_PAGE: &str = "https://github.com/zenvyzenshiro/zenlink/releases";
/// Assets are accepted only from here.
pub const DOWNLOAD_PREFIX: &str = "https://github.com/zenvyzenshiro/zenlink/releases/download/";

/// A version like `2.5.1`, `v2.5.1-beta.1` (semver order: a pre-release is
/// older than its release; numeric parts compare as numbers).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Vec<Pre>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pre {
    Num(u64),
    Text(String),
}

impl Version {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().trim_start_matches(['v', 'V']);
        let s = s.split('+').next()?; // build metadata does not order
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (s, None),
        };
        let mut nums = core.split('.').map(|n| n.parse::<u64>().ok());
        let major = nums.next()??;
        let minor = nums.next().unwrap_or(Some(0))?;
        let patch = nums.next().unwrap_or(Some(0))?;
        if nums.next().is_some() {
            return None;
        }
        let pre = match pre {
            None => Vec::new(),
            Some("") => return None,
            Some(p) => p
                .split('.')
                .map(|id| match id.parse::<u64>() {
                    Ok(n) => Some(Pre::Num(n)),
                    Err(_) if !id.is_empty() => Some(Pre::Text(id.to_ascii_lowercase())),
                    Err(_) => None,
                })
                .collect::<Option<Vec<_>>>()?,
        };
        Some(Self {
            major,
            minor,
            patch,
            pre,
        })
    }

    pub fn is_pre(&self) -> bool {
        !self.pre.is_empty()
    }
}

impl Ord for Pre {
    fn cmp(&self, o: &Self) -> Ordering {
        match (self, o) {
            (Pre::Num(a), Pre::Num(b)) => a.cmp(b),
            (Pre::Num(_), Pre::Text(_)) => Ordering::Less,
            (Pre::Text(_), Pre::Num(_)) => Ordering::Greater,
            (Pre::Text(a), Pre::Text(b)) => a.cmp(b),
        }
    }
}
impl PartialOrd for Pre {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

impl Ord for Version {
    fn cmp(&self, o: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(o.major, o.minor, o.patch))
            .then_with(|| match (self.pre.is_empty(), o.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&o.pre),
            })
    }
}
impl PartialOrd for Version {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        for (i, p) in self.pre.iter().enumerate() {
            f.write_str(if i == 0 { "-" } else { "." })?;
            match p {
                Pre::Num(n) => write!(f, "{n}")?,
                Pre::Text(t) => f.write_str(t)?,
            }
        }
        Ok(())
    }
}

/// Which releases a build follows: `Stable` only full releases, `Beta`
/// also pre-releases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Stable,
    Beta,
}

impl Channel {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "stable" => Some(Self::Stable),
            "beta" => Some(Self::Beta),
            _ => None,
        }
    }
    /// A pre-release build is on the beta channel.
    pub fn of(v: &Version) -> Self {
        if v.is_pre() {
            Self::Beta
        } else {
            Self::Stable
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
    /// Lower-case hex SHA-256 from the release's `digest` field.
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    pub version: Version,
    pub prerelease: bool,
    pub notes: String,
    pub page: String,
    pub setup: Option<Asset>,
    /// `-Setup-Direct.exe` (2.5.14+): the portable exe that installs itself, for PCs where
    /// Smart App Control blocks the Inno Setup.
    pub setup_direct: Option<Asset>,
    pub portable: Option<Asset>,
}

/// What the app offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Offer {
    /// Nothing newer on the chosen channel.
    UpToDate,
    /// A newer version on the build's own channel.
    Update(Release),
    /// The other channel was chosen: its newest release, which may be older
    /// (beta to stable) and installs as the other edition.
    Switch(Release),
}

fn asset(v: &Value) -> Option<Asset> {
    let name = v.get("name")?.as_str()?.to_string();
    let url = v.get("browser_download_url")?.as_str()?.to_string();
    let size = v.get("size")?.as_u64()?;
    let hex = v
        .get("digest")?
        .as_str()?
        .strip_prefix("sha256:")?
        .to_ascii_lowercase();
    let ok = url.starts_with(DOWNLOAD_PREFIX)
        && url.ends_with(&format!("/{name}"))
        && !url[DOWNLOAD_PREFIX.len()..].contains("..")
        && size > 0
        && hex.len() == 64
        && hex.bytes().all(|b| b.is_ascii_hexdigit());
    ok.then_some(Asset {
        name,
        url,
        size,
        sha256: hex,
    })
}

/// One release from the API, `None` for drafts and unparsable tags.
pub fn release(v: &Value) -> Option<Release> {
    if v.get("draft").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    let tag = v.get("tag_name")?.as_str()?.to_string();
    let version = Version::parse(&tag)?;
    let assets: Vec<Asset> = v
        .get("assets")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(asset).collect())
        .unwrap_or_default();
    let find = |suffix: &str| assets.iter().find(|a| a.name.ends_with(suffix)).cloned();
    Some(Release {
        prerelease: v
            .get("prerelease")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || version.is_pre(),
        notes: v
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        page: v
            .get("html_url")
            .and_then(Value::as_str)
            .filter(|u| u.starts_with("https://github.com/zenvyzenshiro/zenlink/"))
            .unwrap_or(RELEASES_PAGE)
            .to_string(),
        setup: find("-Setup.exe"),
        setup_direct: find("-Setup-Direct.exe"),
        portable: find("-Portable.exe"),
        tag,
        version,
    })
}

/// The newest release on `channel`.
pub fn newest(list: &Value, channel: Channel) -> Option<Release> {
    list.as_array()?
        .iter()
        .filter_map(release)
        .filter(|r| channel == Channel::Beta || !r.prerelease)
        .max_by(|a, b| a.version.cmp(&b.version))
}

/// What to offer a build at `current` whose user chose `channel`.
pub fn pick(list: &Value, current: &Version, channel: Channel) -> Offer {
    let Some(r) = newest(list, channel) else {
        return Offer::UpToDate;
    };
    if channel != Channel::of(current) {
        if r.version != *current {
            return Offer::Switch(r);
        }
        return Offer::UpToDate;
    }
    if r.version > *current {
        Offer::Update(r)
    } else {
        Offer::UpToDate
    }
}

/// Release notes as plain text for a small card: Markdown marks dropped,
/// at most `max` characters.
pub fn short_notes(md: &str, max: usize) -> String {
    let mut out = String::new();
    for line in md.lines() {
        let l = line
            .trim()
            .trim_start_matches(['#', '>', '*', '-', ' '])
            .replace("**", "")
            .replace('`', "");
        if l.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(l.trim());
        if out.chars().count() >= max {
            break;
        }
    }
    if out.chars().count() > max {
        out = out.chars().take(max.saturating_sub(1)).collect::<String>() + "…";
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn versions_order_like_semver() {
        assert!(v("2.5.1") > v("2.5.0"));
        assert!(v("2.5.1") > v("2.5.1-beta.9"));
        assert!(v("2.5.1-beta.10") > v("2.5.1-beta.9"));
        assert!(v("2.5.1-beta.1") > v("2.5.0"));
        assert!(v("2.5.1-rc.1") > v("2.5.1-beta.3"));
        assert!(v("2.10.0") > v("2.9.9"));
        assert_eq!(v("v2.5.0"), v("2.5.0"));
        assert_eq!(v("2.5"), v("2.5.0"));
        assert_eq!(v("2.5.1-beta.1").to_string(), "2.5.1-beta.1");
        for bad in ["", "x", "2.x.1", "2.5.1-", "2.5.1-beta..1", "1.2.3.4"] {
            assert!(Version::parse(bad).is_none(), "{bad}");
        }
    }

    fn asset_json(tag: &str, kind: &str) -> Value {
        let name = format!("ZenLink-{tag}-{kind}.exe");
        json!({
            "name": name,
            "size": 40_000_000,
            "browser_download_url": format!("{DOWNLOAD_PREFIX}{tag}/{name}"),
            "digest": format!("sha256:{}", "AB".repeat(32)),
        })
    }

    fn rel(tag: &str, pre: bool) -> Value {
        json!({
            "tag_name": tag, "prerelease": pre, "draft": false,
            "body": "## Fixes\n- **Faster** start\n", "html_url": format!("https://github.com/{REPO}/releases/tag/{tag}"),
            "assets": [asset_json(tag, "Setup"), asset_json(tag, "Portable"), asset_json(tag, "Setup-Direct"), {"name": "x.zip", "size": 1,
                "browser_download_url": format!("{DOWNLOAD_PREFIX}{tag}/x.zip")}],
        })
    }

    fn feed() -> Value {
        json!([
            rel("v2.5.1-beta.2", true),
            rel("v2.5.0", false),
            rel("v2.2.31", false),
            {"tag_name": "v9.9.9", "draft": true, "assets": []},
            {"tag_name": "nightly", "assets": []},
        ])
    }

    #[test]
    fn beta_build_gets_the_newer_beta() {
        match pick(&feed(), &v("2.5.1-beta.1"), Channel::Beta) {
            Offer::Update(r) => {
                assert_eq!(r.version, v("2.5.1-beta.2"));
                let s = r.setup.unwrap();
                assert_eq!(s.name, "ZenLink-v2.5.1-beta.2-Setup.exe");
                assert_eq!(s.sha256, "ab".repeat(32));
                assert_eq!(r.setup_direct.unwrap().name, "ZenLink-v2.5.1-beta.2-Setup-Direct.exe");
                assert!(r.portable.is_some());
            }
            o => panic!("{o:?}"),
        }
        assert_eq!(
            pick(&feed(), &v("2.5.1-beta.2"), Channel::Beta),
            Offer::UpToDate
        );
    }

    #[test]
    fn stable_build_never_sees_betas_or_drafts() {
        assert_eq!(pick(&feed(), &v("2.5.0"), Channel::Stable), Offer::UpToDate);
        match pick(&feed(), &v("2.2.31"), Channel::Stable) {
            Offer::Update(r) => assert_eq!(r.version, v("2.5.0")),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn switching_channel_offers_the_other_edition() {
        match pick(&feed(), &v("2.5.1-beta.1"), Channel::Stable) {
            Offer::Switch(r) => assert_eq!(r.version, v("2.5.0"), "beta to stable may go back"),
            o => panic!("{o:?}"),
        }
        match pick(&feed(), &v("2.5.0"), Channel::Beta) {
            Offer::Switch(r) => assert_eq!(r.version, v("2.5.1-beta.2")),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn assets_from_elsewhere_or_without_digest_are_dropped() {
        let mut r = rel("v2.6.0", false);
        r["assets"][0]["browser_download_url"] =
            json!("https://evil.example/ZenLink-v2.6.0-Setup.exe");
        r["assets"][1]["digest"] = Value::Null;
        let got = release(&r).unwrap();
        assert!(got.setup.is_none());
        assert!(got.portable.is_none());
        let mut r = rel("v2.6.0", false);
        r["assets"][0]["browser_download_url"] = json!(format!(
            "{DOWNLOAD_PREFIX}v2.6.0/../x/ZenLink-v2.6.0-Setup.exe"
        ));
        assert!(release(&r).unwrap().setup.is_none());
        let mut r = rel("v2.6.0", false);
        r["html_url"] = json!("https://evil.example/");
        assert_eq!(release(&r).unwrap().page, RELEASES_PAGE);
    }

    #[test]
    fn notes_become_short_plain_text() {
        assert_eq!(
            short_notes("## Fixes\n- **Faster** start\n\n- `adb` fix\n", 200),
            "Fixes\nFaster start\nadb fix"
        );
        let long = short_notes(&"word ".repeat(100), 20);
        assert_eq!(long.chars().count(), 20);
        assert!(long.ends_with('…'));
    }
}
