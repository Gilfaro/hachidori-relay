// SPDX-License-Identifier: GPL-3.0-or-later
//! What the relay downloads dictionaries from: the recommended catalogue, whose
//! sources update from their pinned URLs (`recommended-dictionaries.js`), the update-source and download trust rules
//! (`managed-dictionary-source.js`), and the HTTP client behind them.

use std::io::Read;
use std::time::Duration;

use percent_encoding::percent_decode_str;
use serde_json::{Value, json};
use url::Url;

use super::js;

/// One trusted starter dictionary (`RECOMMENDED_DICTIONARIES`).
#[derive(Debug)]
pub(crate) struct Recommended {
    pub(crate) source_id: &'static str,
    pub(crate) name: &'static str,
    pub(crate) download_url: &'static str,
    pub(crate) index_url: Option<&'static str>,
    pub(crate) archive_name: &'static str,
    github_repository: Option<&'static str>,
    github_repository_id: Option<&'static str>,
    /// `requiredCapability == "freq"`: the archive must hold frequencies, else terms.
    frequencies: bool,
    title_pattern: fn(&str) -> bool,
    /// `firstInstallOption`: the option its first install selects it for.
    pub(crate) first_install_option: Option<&'static str>,
}

/// `^<prefix> \[\d{4}-\d{2}-\d{2}\]$`.
fn dated(title: &str, prefix: &str) -> bool {
    let Some(rest) = title.strip_prefix(prefix).and_then(|rest| rest.strip_prefix(" [")).and_then(|rest| rest.strip_suffix(']')) else {
        return false;
    };
    let digits = |part: &str, len: usize| part.len() == len && part.chars().all(|c| c.is_ascii_digit());
    let parts: Vec<&str> = rest.split('-').collect();
    matches!(parts[..], [year, month, day] if digits(year, 4) && digits(month, 2) && digits(day, 2))
}

/// The catalogue, in its order.
pub(crate) const RECOMMENDED: [Recommended; 5] = [
    Recommended {
        source_id: "jitendex",
        name: "Jitendex",
        download_url: "https://github.com/stephenmk/stephenmk.github.io/releases/latest/download/jitendex-yomitan.zip",
        index_url: Some("https://jitendex.org/static/yomitan.json"),
        archive_name: "jitendex-yomitan.zip",
        github_repository: Some("stephenmk/stephenmk.github.io"),
        github_repository_id: Some("744330420"),
        frequencies: false,
        title_pattern: |title| dated(title, "Jitendex.org"),
        first_install_option: Some("compactDefinitionSummaryDictionary"),
    },
    Recommended {
        source_id: "jmnedict",
        name: "JMnedict for Yomitan",
        download_url: "https://github.com/yomidevs/jmdict-yomitan/releases/latest/download/JMnedict.zip",
        index_url: Some("https://github.com/yomidevs/jmdict-yomitan/releases/latest/download/JMnedict.json"),
        archive_name: "JMnedict.zip",
        github_repository: Some("yomidevs/jmdict-yomitan"),
        github_repository_id: Some("696075636"),
        frequencies: false,
        title_pattern: |title| dated(title, "JMnedict"),
        first_install_option: None,
    },
    Recommended {
        source_id: "bees-ultimate-kanji-dictionary",
        name: "Bee's Ultimate Kanji Dictionary",
        download_url: "https://github.com/bee-san/bees-ultimate-kanji-dictionary/releases/latest/download/bees-ultimate-kanji-dictionary.zip",
        index_url: Some("https://raw.githubusercontent.com/bee-san/bees-ultimate-kanji-dictionary/main/dist/index.json"),
        archive_name: "bees-ultimate-kanji-dictionary.zip",
        github_repository: Some("bee-san/bees-ultimate-kanji-dictionary"),
        github_repository_id: Some("1335822804"),
        frequencies: false,
        title_pattern: |title| title == "Bee's Ultimate Kanji Dictionary",
        first_install_option: Some("kanjiClickDictionary"),
    },
    Recommended {
        source_id: "jiten",
        name: "Jiten Frequency Dictionary",
        download_url: "https://api.jiten.moe/api/frequency-list/download?downloadType=yomitan",
        index_url: Some("https://api.jiten.moe/api/frequency-list/index"),
        archive_name: "jiten-frequency.zip",
        github_repository: None,
        github_repository_id: None,
        frequencies: true,
        title_pattern: |title| title == "Jiten",
        first_install_option: None,
    },
    Recommended {
        source_id: "bees-ultimate-grammar-dictionary",
        name: "Bee's Ultimate Grammar Dictionary",
        download_url: "https://github.com/bee-san/bees-ultimate-grammar-dictionary/releases/latest/download/bees-ultimate-grammar-dictionary.zip",
        index_url: Some("https://raw.githubusercontent.com/bee-san/bees-ultimate-grammar-dictionary/main/dist/index.json"),
        archive_name: "bees-ultimate-grammar-dictionary.zip",
        github_repository: Some("bee-san/bees-ultimate-grammar-dictionary"),
        github_repository_id: Some("1363159785"),
        frequencies: false,
        title_pattern: |title| title == "Bee's Ultimate Grammar Dictionary",
        first_install_option: None,
    },
];

/// `installedRecommendedDictionary`: the installed package of `source`, by its
/// catalogue source or its exact update index.
pub(crate) fn installed<'a>(source: &Recommended, dictionaries: &'a [Value]) -> Option<&'a Value> {
    dictionaries.iter().find(|dictionary| {
        dictionary.get("sourceId").and_then(Value::as_str) == Some(source.source_id)
            || source.index_url.is_some_and(|url| dictionary.get("indexUrl").and_then(Value::as_str) == Some(url))
    })
}

/// `MANAGED_DICTIONARY_CHANGED`.
pub(crate) const MANAGED_DICTIONARY_CHANGED: &str = "the managed dictionary changed while its update was being prepared";

/// `recommendedDictionarySource`.
pub(crate) fn recommended(source_id: Option<&str>) -> Option<&'static Recommended> {
    RECOMMENDED.iter().find(|entry| Some(entry.source_id) == source_id)
}

/// `httpsUrl`: the URL's `href` when it is HTTPS without credentials.
pub(crate) fn https_url(value: Option<&Value>) -> Option<String> {
    let url = Url::parse(&js::to_string(value)).ok()?;
    (url.scheme() == "https" && url.username().is_empty() && url.password().is_none()).then(|| url.to_string())
}

/// `assertRecommendedDictionary`, of the imported package's report and index.
pub(crate) fn assert_recommended(
    source: &Recommended,
    title: &str,
    index_url: &Value,
    revision: &Value,
    counts: (f64, f64),
) -> Result<(), String> {
    if !(source.title_pattern)(title) {
        return Err(format!("{} archive did not match its expected title", source.name));
    }
    if index_url.as_str() != source.index_url {
        return Err(format!("{} archive did not match its expected update source", source.name));
    }
    if revision.as_str().is_none_or(str::is_empty) {
        return Err(format!("{} archive did not declare a revision", source.name));
    }
    let (terms, frequencies) = counts;
    if (if source.frequencies { frequencies } else { terms }) <= 0.0 {
        return Err(format!("{} archive did not contain its expected capability", source.name));
    }
    Ok(())
}

/// `managedDictionarySource`.
pub(crate) fn managed_source(dictionary: &Value) -> Option<Value> {
    if let Some(entry) = recommended(dictionary.get("sourceId").and_then(Value::as_str)) {
        let index_url = entry.index_url?;
        return Some(
            json!({ "kind": "recommended", "sourceId": entry.source_id, "indexUrl": index_url, "downloadUrl": entry.download_url }),
        );
    }
    let index_url = https_url(dictionary.get("indexUrl"))?;
    let download_url = https_url(dictionary.get("downloadUrl"))?;
    if dictionary.get("isUpdatable") != Some(&Value::Bool(true)) {
        return None;
    }
    let source_id = dictionary.get("sourceId").filter(|id| id.as_str().is_some_and(|id| !id.is_empty())).cloned().unwrap_or(Value::Null);
    Some(json!({ "kind": "generic", "sourceId": source_id, "indexUrl": index_url, "downloadUrl": download_url }))
}

/// `managedDictionaryFingerprint`.
pub(crate) fn fingerprint(dictionary: &Value) -> Option<Value> {
    let source = managed_source(dictionary)?;
    let filled = |name: &str| dictionary.get(name).and_then(Value::as_str).is_some_and(|text| !text.is_empty());
    (filled("id") && filled("path") && dictionary.get("revision").is_some_and(Value::is_string))
        .then(|| json!({ "id": dictionary["id"], "path": dictionary["path"], "revision": dictionary["revision"], "source": source }))
}

/// `managedDictionaryMatches`.
pub(crate) fn matches(dictionary: Option<&Value>, fingerprint: &Value) -> bool {
    let Some(current) = dictionary.and_then(self::fingerprint) else { return false };
    let same = |value: Option<&Value>, path: &[&str]| {
        let other = path.iter().try_fold(fingerprint, |value, key| value.get(*key));
        js::strict_eq(value, other)
    };
    ["id", "path", "revision"].iter().all(|key| same(current.get(*key), &[key]))
        && ["kind", "sourceId", "indexUrl", "downloadUrl"].iter().all(|key| same(current["source"].get(*key), &["source", key]))
}

/// `recommendedAssetUrlMatches`: `value` is the declared URL, or a GitHub release
/// asset of the source's repository with the same file name.
fn asset_url_matches(source: &Recommended, value: Option<&str>, declared: &str, asset: &str) -> bool {
    let Some(final_url) = value.and_then(|value| Url::parse(value).ok()) else { return false };
    if final_url.scheme() != "https" || !final_url.username().is_empty() || final_url.password().is_some() {
        return false;
    }
    if Url::parse(declared).is_ok_and(|declared| declared == final_url) {
        return true;
    }
    let decode = |text: &str| percent_decode_str(text).decode_utf8().map(std::borrow::Cow::into_owned).ok();
    let (Some(repository), Some(repository_id)) = (source.github_repository, source.github_repository_id) else { return false };
    let path = final_url.path();
    match final_url.host_str() {
        Some("github.com") => {
            let rest = path.strip_prefix(&format!("/{repository}/releases/download/")).unwrap_or("");
            rest.contains('/') && decode(&rest[rest.rfind('/').map_or(0, |at| at + 1)..]).as_deref() == Some(asset)
        }
        Some("release-assets.githubusercontent.com") => {
            if !path.starts_with(&format!("/github-production-release-asset/{repository_id}/")) {
                return false;
            }
            let query = |name: &str| final_url.query_pairs().find(|(key, _)| key == name).map(|(_, value)| value.into_owned());
            let disposition = query("response-content-disposition").or_else(|| query("rscd")).unwrap_or_default();
            content_disposition_name(&disposition).as_deref() == Some(asset)
        }
        _ => false,
    }
}

/// `/(?:^|;)\s*filename="?([^";]+)"?/iu`'s capture.
fn content_disposition_name(disposition: &str) -> Option<String> {
    let lower = disposition.to_ascii_lowercase();
    let mut from = 0;
    while let Some(found) = lower[from..].find("filename") {
        let at = from + found;
        let before = lower[..at].trim_end_matches(char::is_whitespace);
        if before.is_empty() || before.ends_with(';') {
            let rest = disposition[at + "filename".len()..].strip_prefix('=');
            if let Some(rest) = rest {
                let rest = rest.strip_prefix('"').unwrap_or(rest);
                let name: String = rest.chars().take_while(|c| *c != '"' && *c != ';').collect();
                if !name.is_empty() {
                    return Some(name);
                }
            }
        }
        from = at + 1;
    }
    None
}

/// `recommendedDownloadUrlMatches`.
pub(crate) fn download_url_matches(source: &Recommended, value: Option<&str>) -> bool {
    asset_url_matches(source, value, source.download_url, source.archive_name)
}

/// `recommendedIndexUrlMatches`.
pub(crate) fn index_url_matches(source: &Recommended, value: Option<&str>) -> bool {
    let Some(index) = source.index_url.and_then(|url| Url::parse(url).ok()) else { return false };
    let path = index.path();
    let name = percent_decode_str(&path[path.rfind('/').map_or(0, |at| at + 1)..]).decode_utf8_lossy().into_owned();
    asset_url_matches(source, value, index.as_str(), &name)
}

/// `MANAGED_UPDATE_SCHEDULE_MINUTES`: each schedule and its interval (none for Off).
const SCHEDULES: [(&str, Option<f64>); 5] = [
    ("off", None),
    ("hourly", Some(60.0)),
    ("daily", Some(24.0 * 60.0)),
    ("weekly", Some(7.0 * 24.0 * 60.0)),
    ("monthly", Some(30.0 * 24.0 * 60.0)),
];

/// `managedUpdateSchedule`.
pub(crate) fn schedule(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str).filter(|schedule| SCHEDULES.iter().any(|(name, _)| name == schedule))
}

/// `normaliseUpdateSettings`.
pub(crate) fn update_settings(value: Option<&Value>) -> Value {
    let revision = js::safe_integer(value.and_then(|value| value.get("revision"))).filter(|revision| *revision >= 0.0).unwrap_or(0.0);
    let last = value.and_then(|value| value.get("lastCheckedAt")).filter(|last| last.is_string()).cloned().unwrap_or(Value::Null);
    json!({
        "revision": js::number(revision),
        "schedule": schedule(value.and_then(|value| value.get("schedule"))).unwrap_or("off"),
        "lastCheckedAt": last,
    })
}

/// `nextDictionaryUpdateCheck`: when `dictionary` is next due, in ms since 1970.
pub(crate) fn next_check(dictionary: &Value, global: &str, now: f64) -> Option<f64> {
    managed_source(dictionary)?;
    let effective = schedule(dictionary.get("updateScheduleOverride")).unwrap_or(global);
    let interval = SCHEDULES.iter().find(|(name, _)| *name == effective).and_then(|(_, minutes)| *minutes)?;
    let checked = dictionary.get("lastUpdateCheck").and_then(|check| check.get("checkedAt")).and_then(Value::as_str).and_then(parse_time);
    Some(checked.map_or(now, |checked| checked + interval * 60_000.0))
}

/// `nextManagedUpdateCheck`.
pub(crate) fn next_managed_check(dictionaries: &[Value], global: &str, now: f64) -> Option<f64> {
    dictionaries.iter().filter_map(|dictionary| next_check(dictionary, global, now)).reduce(f64::min).map(|next| next.max(now))
}

/// `Date.parse` of an ISO time (`YYYY-MM-DDTHH:MM[:SS[.sss]](Z|±HH:MM)`, or a bare date).
pub(crate) fn parse_time(text: &str) -> Option<f64> {
    let digits = |part: &str| (!part.is_empty() && part.chars().all(|c| c.is_ascii_digit())).then(|| part.parse::<i64>().ok()).flatten();
    let (date, time) = text.split_once('T').map_or((text, None), |(date, time)| (date, Some(time)));
    let mut parts = date.splitn(3, '-');
    let (year, month, day) = (parts.next()?, parts.next().unwrap_or("01"), parts.next().unwrap_or("01"));
    if year.len() != 4 || month.len() != 2 || day.len() != 2 {
        return None;
    }
    let (year, month, day) = (digits(year)?, digits(month)?, digits(day)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (mut millis, mut offset) = (0_i64, 0_i64);
    if let Some(time) = time {
        let (clock, zone) = if let Some(clock) = time.strip_suffix('Z') {
            (clock, 0)
        } else {
            let at = time.rfind(['+', '-'])?;
            let (hours, minutes) = time[at + 1..].split_once(':')?;
            let sign = if time.as_bytes()[at] == b'-' { -1 } else { 1 };
            (&time[..at], sign * (digits(hours)? * 60 + digits(minutes)?))
        };
        let (clock, fraction) = clock.split_once('.').map_or((clock, None), |(clock, fraction)| (clock, Some(fraction)));
        let fields: Vec<&str> = clock.split(':').collect();
        let (hours, minutes, seconds) = match fields[..] {
            [hours, minutes] => (digits(hours)?, digits(minutes)?, 0),
            [hours, minutes, seconds] => (digits(hours)?, digits(minutes)?, digits(seconds)?),
            _ => return None,
        };
        if hours > 24 || minutes > 59 || seconds > 59 {
            return None;
        }
        let fraction = match fraction {
            Some(fraction) => digits(&format!("{fraction:0<3}")[..3])?,
            None => 0,
        };
        millis = ((hours * 60 + minutes) * 60 + seconds) * 1000 + fraction;
        offset = zone;
    }
    // Howard Hinnant's days_from_civil.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    #[allow(clippy::cast_precision_loss, reason = "a JavaScript time is within ±8.64e15 ms")]
    let time = (days * 86_400_000 + millis - offset * 60_000) as f64;
    Some(time)
}

/// A response being read.
pub(crate) struct Fetched {
    pub(crate) status: u16,
    /// `response.url`: where the redirects ended.
    pub(crate) url: String,
    /// `declaredResponseLength`: the Content-Length, when the body is not
    /// content-encoded (a decoded body is not comparable with it).
    pub(crate) total: Option<u64>,
    pub(crate) body: Box<dyn Read + Send>,
}

/// `fetch(url, { credentials: "omit" })`. A trait so tests can serve the downloads.
pub(crate) trait Fetch: Send + Sync {
    /// `idle` bounds the wait for the response's headers and each read of its body.
    fn get(&self, url: &str, idle: Option<Duration>) -> Result<Fetched, String>;
}

/// Whether a body read failed because the response went quiet for its idle time.
pub(crate) fn read_timed_out(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::TimedOut
        || error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<ureq::Error>())
            .is_some_and(|inner| matches!(inner, ureq::Error::Timeout(_)))
}

/// The network.
pub(crate) struct Http;

/// What a failed `fetch` rejects with.
pub(crate) const FAILED_TO_FETCH: &str = "Failed to fetch";
/// What a download that went quiet for its idle time fails with.
pub(crate) const TIMED_OUT: &str = "timed out";

impl Fetch for Http {
    fn get(&self, url: &str, idle: Option<Duration>) -> Result<Fetched, String> {
        use ureq::ResponseExt;
        let agent = crate::host::loopback::agent(
            ureq::Agent::config_builder()
                .max_redirects(20)
                .http_status_as_error(false)
                .timeout_connect(Some(Duration::from_secs(30)))
                .timeout_recv_response(idle)
                // Each read renews the wait, with no total deadline: the extension
                // restarts its inactivity timer on every chunk of the body.
                .timeout_per_read(idle)
                .build(),
        );
        let response = agent.get(url).call().map_err(|error| {
            tracing::debug!(%error, %url, "a dictionary download failed");
            if matches!(error, ureq::Error::Timeout(_)) { TIMED_OUT } else { FAILED_TO_FETCH }.to_owned()
        })?;
        let header = |name: &str| response.headers().get(name).and_then(|value| value.to_str().ok()).map(str::to_owned);
        let identity =
            header("content-encoding").is_none_or(|encoding| encoding.is_empty() || encoding.trim().eq_ignore_ascii_case("identity"));
        let total = header("content-length").and_then(|length| length.trim().parse::<u64>().ok()).filter(|length| identity && *length > 0);
        Ok(Fetched {
            status: response.status().as_u16(),
            url: response.get_uri().to_string(),
            total,
            body: Box::new(response.into_body().into_reader()),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    /// A loopback server answering once: headers, then `chunks` of body each
    /// after `gap`, then (if `stall`) silence instead of the end.
    fn trickle(chunks: usize, gap: Duration, stall: bool) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/index.json", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let _ = Read::read(&mut socket, &mut request);
            let length = if stall { chunks + 1 } else { chunks };
            let _ = write!(
                socket,
                "HTTP/1.1 200 OK
content-length: {length}
connection: close

"
            );
            for _ in 0..chunks {
                std::thread::sleep(gap);
                let _ = socket.write_all(b"x");
                let _ = socket.flush();
            }
            if stall {
                std::thread::sleep(gap * 20);
            }
        });
        url
    }

    // The extension's update-check timeout: slow chunks renew the inactivity
    // wait with no total deadline, and silence for the whole wait fails.
    #[test]
    fn a_slow_body_renews_its_idle_wait_and_silence_times_out() {
        let idle = Some(Duration::from_millis(400));
        let mut body = Vec::new();
        let fetched = Http.get(&trickle(8, Duration::from_millis(150), false), idle).unwrap();
        assert_eq!(fetched.total, Some(8), "the declared length of an unencoded body");
        fetched.body.take(64).read_to_end(&mut body).unwrap();
        assert_eq!(body, b"xxxxxxxx", "1.2 s of body in 150 ms chunks is within a 400 ms idle wait");
        let fetched = Http.get(&trickle(1, Duration::from_millis(100), true), idle).unwrap();
        let error = fetched.body.take(64).read_to_end(&mut Vec::new()).unwrap_err();
        assert!(read_timed_out(&error), "{error:?}");
    }

    #[test]
    fn recommended_downloads_may_end_at_their_release_assets() {
        let jitendex = recommended(Some("jitendex")).unwrap();
        assert!(download_url_matches(jitendex, Some(jitendex.download_url)));
        assert!(download_url_matches(
            jitendex,
            Some("https://github.com/stephenmk/stephenmk.github.io/releases/download/v2025/jitendex-yomitan.zip")
        ));
        assert!(!download_url_matches(jitendex, Some("https://github.com/someone/else/releases/download/v1/jitendex-yomitan.zip")));
        assert!(download_url_matches(
            jitendex,
            Some(
                "https://release-assets.githubusercontent.com/github-production-release-asset/744330420/abc?response-content-disposition=attachment%3B%20filename%3Djitendex-yomitan.zip"
            )
        ));
        assert!(!download_url_matches(
            jitendex,
            Some(
                "https://release-assets.githubusercontent.com/github-production-release-asset/1/abc?rscd=attachment%3B%20filename%3Djitendex-yomitan.zip"
            )
        ));
        assert!(!download_url_matches(
            jitendex,
            Some("http://github.com/stephenmk/stephenmk.github.io/releases/latest/download/jitendex-yomitan.zip")
        ));
        let jmnedict = recommended(Some("jmnedict")).unwrap();
        assert!(index_url_matches(jmnedict, Some("https://github.com/yomidevs/jmdict-yomitan/releases/download/2025-01-01/JMnedict.json")));
        let jiten = recommended(Some("jiten")).unwrap();
        assert!(!download_url_matches(jiten, Some("https://api.jiten.moe/elsewhere")));
    }

    #[test]
    fn catalogue_titles_follow_their_patterns() {
        let check = |id: &str, title: &str| {
            assert_recommended(
                recommended(Some(id)).unwrap(),
                title,
                &json!(recommended(Some(id)).unwrap().index_url),
                &json!("1"),
                (1.0, 1.0),
            )
        };
        assert!(check("jitendex", "Jitendex.org [2025-01-31]").is_ok());
        assert!(check("jitendex", "Jitendex.org [2025-1-31]").is_err());
        assert!(check("jiten", "Jiten").is_ok());
        let jiten = recommended(Some("jiten")).unwrap();
        assert_eq!(
            assert_recommended(jiten, "Jiten", &json!(jiten.index_url), &json!("1"), (5.0, 0.0)).unwrap_err(),
            "Jiten Frequency Dictionary archive did not contain its expected capability"
        );
    }

    #[test]
    fn schedules_count_from_the_last_check() {
        let dictionary = json!({ "id": "a", "path": "/p", "revision": "1", "isUpdatable": true,
            "indexUrl": "https://x/index.json", "downloadUrl": "https://x/d.zip",
            "lastUpdateCheck": { "checkedAt": "2025-01-01T00:00:00.000Z" } });
        let checked = parse_time("2025-01-01T00:00:00.000Z").unwrap();
        assert_eq!(js::iso_time(checked).as_deref(), Some("2025-01-01T00:00:00.000Z"));
        assert_eq!(next_check(&dictionary, "daily", 0.0), Some(checked + 86_400_000.0));
        assert_eq!(next_check(&dictionary, "off", 0.0), None);
        let mut hourly = dictionary.clone();
        hourly["updateScheduleOverride"] = json!("hourly");
        assert_eq!(next_check(&hourly, "off", 0.0), Some(checked + 3_600_000.0));
        assert_eq!(next_managed_check(&[dictionary, hourly], "weekly", checked + 1e10), Some(checked + 1e10));
        assert_eq!(parse_time("2025-01-01T01:00+01:00"), Some(checked));
        assert_eq!(
            update_settings(Some(&json!({ "revision": -1, "schedule": "yearly" }))),
            json!({ "revision": 0, "schedule": "off", "lastCheckedAt": null })
        );
    }

    /// Upstream's dictionary-update-schedule.test.mjs at bf1a78f, with its values.
    mod upstream_schedules {
        use super::*;

        const HOUR: f64 = 3_600_000.0;

        fn now() -> f64 {
            parse_time("2026-09-07T12:00:00Z").unwrap()
        }

        fn managed(patch: Value) -> Value {
            let Value::Object(patch) = patch else { panic!("a patch is an object") };
            let mut dictionary = json!({ "isUpdatable": true, "indexUrl": "https://example.com/index.json",
                "downloadUrl": "https://example.com/dictionary.zip" });
            for (key, value) in patch {
                dictionary[key] = value;
            }
            dictionary
        }

        fn at(offset: f64) -> String {
            js::iso_time(now() + offset).unwrap()
        }

        #[test]
        fn explicit_policies_override_the_global_schedule() {
            let now = now();
            assert_eq!(next_check(&managed(json!({})), "daily", now), Some(now));
            assert_eq!(next_check(&managed(json!({ "updateScheduleOverride": null })), "weekly", now), Some(now));
            let checked = json!({ "lastUpdateCheck": { "checkedAt": at(0.0) } });
            let hourly = managed(json!({ "updateScheduleOverride": "hourly", "lastUpdateCheck": checked["lastUpdateCheck"] }));
            assert_eq!(next_check(&hourly, "off", now), Some(now + HOUR), "hourly overrides global Off");
            let weekly = managed(json!({ "updateScheduleOverride": null, "lastUpdateCheck": checked["lastUpdateCheck"] }));
            assert_eq!(next_check(&weekly, "weekly", now), Some(now + 168.0 * HOUR), "null inherits");
            assert_eq!(next_check(&managed(json!({ "updateScheduleOverride": "off" })), "hourly", now), None);
            assert_eq!(next_check(&json!({ "isUpdatable": false }), "hourly", now), None);
        }

        #[test]
        fn due_times_include_disabled_packages_and_advance_after_any_attempt() {
            let now = now();
            for (schedule, hours) in [("hourly", 1.0), ("daily", 24.0), ("weekly", 168.0), ("monthly", 720.0)] {
                for status in ["up-to-date", "check-failed", "update-available"] {
                    let dictionary = managed(json!({ "enabled": false, "updateScheduleOverride": schedule,
                        "lastUpdateCheck": { "checkedAt": at(-HOUR), "status": status } }));
                    assert_eq!(next_check(&dictionary, "off", now), Some(now + (hours - 1.0) * HOUR), "{schedule} {status}");
                }
            }
            for checked_at in [None, Some(json!(null)), Some(json!("invalid"))] {
                let check = checked_at.map_or_else(|| json!({}), |checked_at| json!({ "checkedAt": checked_at }));
                assert_eq!(next_check(&managed(json!({ "lastUpdateCheck": check })), "daily", now), Some(now));
            }
        }

        #[test]
        fn one_aggregate_due_time_selects_only_managed_active_schedules() {
            let now = now();
            let checked = json!({ "checkedAt": at(0.0) });
            assert_eq!(next_managed_check(&[], "hourly", now), None);
            assert_eq!(next_managed_check(&[managed(json!({ "updateScheduleOverride": "off" }))], "daily", now), None);
            let dictionaries = vec![
                managed(json!({ "lastUpdateCheck": checked })),
                managed(json!({ "enabled": false, "updateScheduleOverride": "hourly", "lastUpdateCheck": checked })),
                json!({ "isUpdatable": false }),
            ];
            assert_eq!(next_managed_check(&dictionaries, "daily", now), Some(now + HOUR));
            assert_eq!(next_managed_check(&dictionaries, "off", now), Some(now + HOUR));
            let unchecked = [dictionaries, vec![managed(json!({}))]].concat();
            assert_eq!(next_managed_check(&unchecked, "daily", now), Some(now));
            let overdue = managed(json!({ "lastUpdateCheck": { "checkedAt": at(-48.0 * HOUR) } }));
            assert_eq!(next_managed_check(&[overdue], "daily", now), Some(now));
        }
    }

    #[test]
    fn fingerprints_name_the_source_they_update_from() {
        let generic = json!({ "id": "a", "path": "/p", "revision": "1", "isUpdatable": true,
            "indexUrl": "https://x/index.json", "downloadUrl": "https://x/d.zip" });
        let print = fingerprint(&generic).unwrap();
        assert_eq!(print["source"]["kind"], "generic");
        assert!(matches(Some(&generic), &print));
        let mut changed = generic.clone();
        changed["revision"] = json!("2");
        assert!(!matches(Some(&changed), &print));
        let mut plain = generic;
        plain["isUpdatable"] = json!(false);
        assert!(fingerprint(&plain).is_none());
        let jitendex = json!({ "id": "j", "path": "/j", "revision": "1", "sourceId": "jitendex" });
        assert_eq!(fingerprint(&jitendex).unwrap()["source"]["downloadUrl"], RECOMMENDED[0].download_url);
    }
}
