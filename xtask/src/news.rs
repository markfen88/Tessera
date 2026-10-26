use crate::build_bundle::text;
use crate::{Failure, Result, env_or, run};
use std::fs::{self, File};
use std::path::Path;
use std::process::Command;

fn entry_version<'a>(text: &'a str, package: &str) -> Result<&'a str> {
    let prefix = format!("{package} (");
    text.lines()
        .next()
        .and_then(|line| line.strip_prefix(prefix.as_str()))
        .and_then(|line| line.split_once(") "))
        .map(|(version, _)| version)
        .filter(|version| !version.is_empty())
        .ok_or_else(|| {
            Failure::new(format!(
                "NEWS must start with a changelog header line: {package} (VERSION) ..."
            ))
        })
}

pub(crate) fn write_news(
    directory: &Path,
    package: &str,
    source: &Path,
    build: &str,
) -> Result<()> {
    let text = fs::read_to_string(source)?;
    let version = entry_version(&text, package)?;
    let ceiling = build.split("~g").next().unwrap_or(build);
    let comparison = Command::new("dpkg")
        .args(["--compare-versions", version, "le", ceiling])
        .status()?;
    if !comparison.success() {
        return Err(Failure::new(format!(
            "{} is for version {version}, newer than release {ceiling} (build {build}): set its first line to the version of this release",
            source.display()
        )));
    }
    let destination = directory.join("usr/share/doc").join(package);
    fs::create_dir_all(&destination)?;
    run(Command::new("gzip")
        .arg("-9n")
        .stdin(File::open(source)?)
        .stdout(File::create(destination.join("NEWS.Debian.gz"))?))
}

/// Retain existing versioned NEWS entries in snapshot changelogs, so lintian can
/// resolve their versions. A preview must never gain a newer final entry.
pub(crate) fn changelog_history(
    source: &Path,
    package: &str,
    build: &str,
    maintainer: &str,
) -> Result<String> {
    let text = fs::read_to_string(source)?;
    let prefix = format!("{package} (");
    let mut entries = Vec::new();
    let mut entry = String::new();
    for line in text.split_inclusive('\n') {
        if line.starts_with(&prefix) && !entry.is_empty() {
            entries.push(std::mem::take(&mut entry));
        }
        entry.push_str(line);
    }
    if !entry.is_empty() {
        entries.push(entry);
    }
    let mut history = String::new();
    for entry in entries {
        let version = entry_version(&entry, package)?;
        if Command::new("dpkg")
            .args(["--compare-versions", version, "lt", build])
            .status()?
            .success()
        {
            // The source NEWS remains byte-for-byte unchanged, including its
            // original placeholder. Generated changelogs use the package contact.
            history.push_str(&entry.replace("tessera maintainers <tessera@invalid>", maintainer));
        }
    }
    Ok(history)
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The inverse of [`days_from_civil`]: (year, month, day).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Seconds since the epoch of an RFC 2822 date as a changelog trailer has it
/// (`Mon, 26 Oct 2026 00:00:00 +0000`; the weekday is optional and not checked).
fn rfc2822_epoch(date: &str) -> Option<i64> {
    let rest = date.split_once(", ").map_or(date, |(_, rest)| rest);
    let mut parts = rest.split_whitespace();
    let day: i64 = parts.next()?.parse().ok()?;
    let name = parts.next()?;
    let month = i64::try_from(MONTHS.iter().position(|month| *month == name)?).ok()? + 1;
    let year: i64 = parts.next()?.parse().ok()?;
    let mut clock = parts.next()?.split(':');
    let hours: i64 = clock.next()?.parse().ok()?;
    let minutes: i64 = clock.next()?.parse().ok()?;
    let seconds: i64 = clock.next().map_or(Some(0), |s| s.parse().ok())?;
    let zone = parts.next()?;
    let (sign, digits) = match zone.split_at_checked(1)? {
        ("+", digits) => (1, digits),
        ("-", digits) => (-1, digits),
        _ => return None,
    };
    if digits.len() != 4 || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let offset =
        sign * (digits[..2].parse::<i64>().ok()? * 3600 + digits[2..].parse::<i64>().ok()? * 60);
    Some(
        days_from_civil(year, month, day) * 86_400 + hours * 3600 + minutes * 60 + seconds - offset,
    )
}

/// `2026-10-26 03:46:00 UTC` for the message.
fn utc_text(epoch: i64) -> String {
    let (year, month, day) = civil_from_days(epoch.div_euclid(86_400));
    let seconds = epoch.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60
    )
}

/// The versions of `package`'s entries in `text` that are dated after `epoch`,
/// with the date as written. A generated changelog dates its own entry
/// `epoch` and puts these entries below it, and lintian refuses a changelog
/// whose newest entry is older than the one before it.
fn entries_dated_after(text: &str, package: &str, epoch: i64) -> Vec<(String, String)> {
    let prefix = format!("{package} (");
    let mut version = None;
    let mut later = Vec::new();
    for line in text.lines() {
        if let Some(header) = line.strip_prefix(prefix.as_str()) {
            version = header.split_once(") ").map(|(version, _)| version);
        } else if let (Some(trailer), Some(version)) = (line.strip_prefix(" -- "), version) {
            let date = trailer
                .rsplit_once("  ")
                .map_or("", |(_, date)| date.trim());
            if rfc2822_epoch(date).is_some_and(|dated| dated > epoch) {
                later.push((version.to_owned(), date.to_owned()));
            }
        }
    }
    later
}

/// Fail with a plain message when a `packaging/NEWS.Debian` entry is dated
/// later than the time the packages are built with (`SOURCE_DATE_EPOCH`,
/// else the last commit's time, as `build-deb` takes it).
pub(crate) fn check_dates(root: &Path) -> Result<()> {
    let epoch = env_or(
        "SOURCE_DATE_EPOCH",
        &text(
            Command::new("git")
                .current_dir(root)
                .args(["log", "-1", "--format=%ct"]),
        )
        .unwrap_or_default(),
    );
    let Ok(epoch) = epoch.parse::<i64>() else {
        // build-deb reports an invalid or missing epoch itself.
        return Ok(());
    };
    check_dates_at(&root.join("packaging/NEWS.Debian"), epoch)
}

fn check_dates_at(source: &Path, epoch: i64) -> Result<()> {
    let later = entries_dated_after(&fs::read_to_string(source)?, "tessera", epoch);
    if later.is_empty() {
        return Ok(());
    }
    let entries = later
        .iter()
        .map(|(version, date)| format!("{version} ({date})"))
        .collect::<Vec<_>>()
        .join(", ");
    Err(Failure::new(format!(
        "{} has an entry dated later than the build time {}: {entries}. The package changelog dates its own entry with SOURCE_DATE_EPOCH (else the commit time) and lintian refuses a newest entry older than the one below it (latest-changelog-entry-without-new-date). Date the NEWS entry no later than the release commit, or build with a later SOURCE_DATE_EPOCH.",
        source.display(),
        utc_text(epoch)
    )))
}
