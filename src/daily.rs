//! The daily block on index.html: today's date and who has a name day.
//!
//! Only the text between the two markers may change from day to day. Before hashing,
//! the monitor replaces that text with `{{DAILY}}`, but only if it is exactly the text
//! the calendar prescribes for today. Anything else in the block is an incident.
use anyhow::{bail, ensure, Context, Result};
use chrono::{DateTime, Duration, NaiveDate, Timelike};
use chrono_tz::Tz;
use std::{collections::BTreeMap, fs, path::Path};

pub const START: &str = "<!-- FIM:daily -->";
pub const END: &str = "<!-- /FIM:daily -->";
pub const TOKEN: &str = "{{DAILY}}";
/// Right after midnight the page timer may not have run yet, so yesterday's block
/// is still accepted for this many seconds.
pub const GRACE_SECONDS: u32 = 120;

/// "MM-DD" -> names; an empty string means nobody has a name day (public holidays).
pub type Names = BTreeMap<String, String>;

pub fn load_names(path: &Path) -> Result<Names> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("cannot read calendar {}", path.display()))?;
    let names: Names = serde_json::from_str(&text).context("invalid namedays.json")?;
    ensure!(
        names.len() == 366,
        "namedays.json must contain exactly 366 days"
    );
    // 2000 is a leap year, so walking through it visits every possible MM-DD.
    let mut day = NaiveDate::from_ymd_opt(2000, 1, 1).context("bad date")?;
    while day.format("%Y").to_string() == "2000" {
        ensure!(
            names.contains_key(&key(day)),
            "namedays.json misses {}",
            key(day)
        );
        day += Duration::days(1);
    }
    Ok(names)
}

pub fn load_template(path: &Path) -> Result<String> {
    let html = fs::read_to_string(path)
        .with_context(|| format!("cannot read template {}", path.display()))?;
    ensure!(
        html.matches(START).count() == 1
            && html.matches(END).count() == 1
            && html.matches(TOKEN).count() == 1
            && html.contains(&format!("{START}{TOKEN}{END}")),
        "template must contain exactly one {START}{TOKEN}{END}"
    );
    Ok(html)
}

fn key(date: NaiveDate) -> String {
    date.format("%m-%d").to_string()
}

/// The text shown on the page, with the Slovak date format `22. 11. 2026`.
pub fn text(names: &Names, date: NaiveDate) -> Result<String> {
    let who = names.get(&key(date)).context("date missing in calendar")?;
    let who = who
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let date = date.format("%-d. %-m. %Y");
    Ok(if who.is_empty() {
        format!("Dnes je {date}. Dnes nemá meniny nikto.")
    } else {
        format!("Dnes je {date}. Meniny má {who}.")
    })
}

pub fn render(template: &str, names: &Names, date: NaiveDate) -> Result<String> {
    Ok(template.replacen(TOKEN, &text(names, date)?, 1))
}

/// Splits the page into (before the block, block content, after the block).
fn split(html: &str) -> Result<(&str, &str, &str)> {
    ensure!(
        html.matches(START).count() == 1 && html.matches(END).count() == 1,
        "page must contain exactly one daily block"
    );
    let from = html.find(START).context("no start marker")? + START.len();
    let to = html.find(END).context("no end marker")?;
    ensure!(from <= to, "daily markers are in the wrong order");
    Ok((&html[..from], &html[from..to], &html[to..]))
}

/// Replaces any block content with the token. Used by `page` to compare the rest of the page.
pub fn strip(html: &str) -> Result<String> {
    let (before, _, after) = split(html)?;
    Ok(format!("{before}{TOKEN}{after}"))
}

/// Like `strip`, but only if the block holds the correct text for one of `dates`.
pub fn normalize(html: &str, names: &Names, dates: &[NaiveDate]) -> Result<String> {
    let (_, block, _) = split(html)?;
    for date in dates {
        if block == text(names, *date)? {
            return strip(html);
        }
    }
    bail!("daily block does not match today's date and name day")
}

/// Today, plus yesterday during the first GRACE_SECONDS after midnight.
pub fn accepted_dates(now: DateTime<Tz>) -> Vec<NaiveDate> {
    let today = now.date_naive();
    if now.num_seconds_from_midnight() < GRACE_SECONDS {
        vec![today, today - Duration::days(1)]
    } else {
        vec![today]
    }
}
