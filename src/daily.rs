use anyhow::{ensure, Context, Result};
use chrono::{DateTime, NaiveDate, Timelike};
use chrono_tz::Tz;
use std::collections::BTreeMap;
pub type Names = BTreeMap<String, String>;
pub const START: &str = "<!-- FIM:daily -->";
pub const END: &str = "<!-- /FIM:daily -->";
pub const TOKEN: &str = "{{DAILY}}";

pub fn validate_calendar(names: &Names) -> Result<()> {
    ensure!(
        names.len() == 366,
        "Calendar must contain exactly 366 dates"
    );
    let mut d = NaiveDate::from_ymd_opt(2000, 1, 1).context("Invalid reference date")?;
    for _ in 0..366 {
        let value = names
            .get(&d.format("%m-%d").to_string())
            .context("Calendar date missing")?;
        ensure!(
            !value.is_empty() && value.len() <= 1024,
            "Invalid calendar name"
        );
        d = d.succ_opt().context("Date overflow")?;
    }
    Ok(())
}
pub fn value(names: &Names, date: NaiveDate) -> Result<String> {
    let name = names
        .get(&date.format("%m-%d").to_string())
        .context("Calendar date missing")?;
    let safe = name
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;");
    Ok(format!("Dátum: {date}. Meniny: {safe}."))
}
/// Every calendar day must fit, including entries longer than today's name day.
pub fn validate_rendered_size(template: &str, names: &Names, limit: u64) -> Result<()> {
    ensure!(
        template.matches(TOKEN).count() == 1,
        "Template needs one DAILY token"
    );
    let fixed = template.len() - TOKEN.len();
    let mut date = NaiveDate::from_ymd_opt(2000, 1, 1).context("Invalid reference date")?;
    for _ in 0..366 {
        ensure!(
            (fixed + value(names, date)?.len()) as u64 <= limit,
            "Rendered index exceeds size limit on {}",
            date.format("%m-%d")
        );
        date = date.succ_opt().context("Date overflow")?;
    }
    Ok(())
}
pub fn render(template: &str, names: &Names, date: NaiveDate, limit: u64) -> Result<String> {
    let html = template.replace(TOKEN, &value(names, date)?);
    ensure!(
        html.len() as u64 <= limit,
        "Rendered index exceeds size limit"
    );
    Ok(html)
}
pub fn normalize_dates(html: &str, names: &Names, dates: &[NaiveDate]) -> Result<String> {
    ensure!(
        html.matches(START).count() == 1 && html.matches(END).count() == 1,
        "Expected exactly one daily block"
    );
    let from = html.find(START).context("No start")? + START.len();
    let to = html.find(END).context("No end")?;
    ensure!(from <= to, "Reversed daily markers");
    let mut valid = false;
    for date in dates {
        valid |= html[from..to] == value(names, *date)?;
    }
    ensure!(valid, "Incorrect date, name day or injected daily content");
    Ok(format!("{}{}{}", &html[..from], TOKEN, &html[to..]))
}
/// Time is sampled immediately before and after THIS file read, not before the tree scan.
pub fn normalize(
    html: &str,
    names: &Names,
    begin: DateTime<Tz>,
    end: DateTime<Tz>,
    grace: u32,
) -> Result<String> {
    ensure!(
        (end - begin).num_seconds().abs() <= 300,
        "Clock jump or excessively slow index read"
    );
    let mut dates = vec![begin.date_naive(), end.date_naive()];
    for t in [begin, end] {
        if t.num_seconds_from_midnight() < grace {
            dates.push(t.date_naive().pred_opt().context("Date underflow")?);
        }
    }
    normalize_dates(html, names, &dates)
}
