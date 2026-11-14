use anyhow::{Context, Result, ensure};
use chrono::{DateTime, NaiveDate, SecondsFormat};
use orbment_core::i18n::tr;
use rusqlite::{Connection, params};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrafficDay {
    pub upload: i64,
    pub download: i64,
}

/// Monitor counters are KiB. Duplicate dates in one file represent additive records.
pub fn parse_traffic(text: &str) -> Result<BTreeMap<NaiveDate, TrafficDay>> {
    let mut lines = text
        .trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let header = lines.next().context(tr("importer.invalid_source"))?;
    let count: usize = header
        .strip_prefix("lines:")
        .unwrap_or(header)
        .trim()
        .trim_matches('"')
        .parse()?;
    let mut days = BTreeMap::<NaiveDate, TrafficDay>::new();
    let mut seen = 0;
    for line in lines {
        seen += 1;
        let mut fields = line.split_whitespace();
        let date = NaiveDate::parse_from_str(
            fields.next().context(tr("importer.invalid_source"))?,
            "%Y/%m/%d",
        )?;
        let counts = fields.next().context(tr("importer.invalid_source"))?;
        ensure!(
            fields.next().is_none(),
            "{}: {line}",
            tr("importer.invalid_source")
        );
        // Older combined-counter records cannot recover direction; retain their total as download.
        let (up, down) = counts.split_once('/').unwrap_or(("0", counts));
        let bytes = |value: &str| -> Result<i64> {
            value
                .parse::<i64>()?
                .checked_mul(1024)
                .filter(|v| *v >= 0)
                .context(tr("importer.invalid_source"))
        };
        let day = days.entry(date).or_default();
        day.upload = day
            .upload
            .checked_add(bytes(up)?)
            .context(tr("importer.invalid_source"))?;
        day.download = day
            .download
            .checked_add(bytes(down)?)
            .context(tr("importer.invalid_source"))?;
    }
    ensure!(
        seen == count,
        "{}: {count}/{seen}",
        tr("importer.invalid_source")
    );
    Ok(days)
}

/// Backups, other installations, and the existing Orbment day are overlapping snapshots.
pub fn merge_traffic(
    target: &mut BTreeMap<NaiveDate, TrafficDay>,
    source: BTreeMap<NaiveDate, TrafficDay>,
) {
    for (date, incoming) in source {
        let day = target.entry(date).or_default();
        day.upload = day.upload.max(incoming.upload);
        day.download = day.download.max(incoming.download);
    }
}

pub fn write_traffic(
    conn: &Connection,
    days: &BTreeMap<NaiveDate, TrafficDay>,
    now: &str,
) -> Result<()> {
    let mut statement = conn.prepare(
        "INSERT INTO traffic_daily_history(day_date,upload_bytes,download_bytes,adapter_mode,updated_at)
         VALUES(?1,?2,?3,'auto',?4) ON CONFLICT(day_date) DO UPDATE SET
         upload_bytes=MAX(upload_bytes,excluded.upload_bytes),
         download_bytes=MAX(download_bytes,excluded.download_bytes),updated_at=excluded.updated_at",
    )?;
    for (date, day) in days {
        statement.execute(params![date.to_string(), day.upload, day.download, now])?;
    }
    Ok(())
}

pub fn import_everything(conn: &Connection, bytes: &[u8]) -> Result<usize> {
    let text = crate::source::decode_text(bytes)?;
    let mut reader = csv::Reader::from_reader(text.as_bytes());
    let headers = reader.headers()?.clone();
    let column = |name| {
        headers
            .iter()
            .position(|h| h == name)
            .with_context(|| format!("{}: {name}", tr("importer.invalid_source")))
    };
    let path = column("Filename")?;
    let count = column("Run Count")?;
    let date = column("Last Run Date")?;
    let mut statement = conn.prepare(
        "INSERT INTO file_run_history(full_path,run_count,last_run_date) VALUES(?1,?2,?3)
         ON CONFLICT(full_path) DO UPDATE SET run_count=MAX(run_count,excluded.run_count),
         last_run_date=MAX(last_run_date,excluded.last_run_date)",
    )?;
    let mut rows = 0;
    for record in reader.records() {
        let record = record?;
        ensure!(
            !record[path].trim().is_empty(),
            "{}",
            tr("importer.invalid_source")
        );
        let runs: i64 = record[count].parse()?;
        ensure!(runs >= 0, "{}", tr("importer.invalid_source"));
        let ticks: u64 = record[date].parse()?;
        let seconds = i64::try_from(ticks / 10_000_000)? - 11_644_473_600;
        let utc = DateTime::from_timestamp(seconds, ((ticks % 10_000_000) * 100) as u32)
            .context(tr("importer.invalid_source"))?
            .to_rfc3339_opts(SecondsFormat::Millis, true);
        statement.execute(params![&record[path], runs, utc])?;
        rows += 1;
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_within_file_then_takes_independent_direction_maxima() {
        let mut merged = parse_traffic("lines: 2\n2026/10/20 2/9\n2026/10/20 3/1\n").unwrap();
        let backup = parse_traffic("1\n2026/10/20 7/8\n").unwrap();
        merge_traffic(&mut merged, backup.clone());
        merge_traffic(&mut merged, backup);
        let day = &merged[&NaiveDate::from_ymd_opt(2026, 10, 20).unwrap()];
        assert_eq!(
            *day,
            TrafficDay {
                upload: 7 * 1024,
                download: 10 * 1024
            }
        );
        let existing = parse_traffic("1\n2026/10/20 4/12\n").unwrap();
        merge_traffic(&mut merged, existing);
        assert_eq!(merged.values().next().unwrap().download, 12 * 1024);
    }
}
