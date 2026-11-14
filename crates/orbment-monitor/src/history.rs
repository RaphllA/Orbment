use std::{collections::BTreeMap, path::Path};

use anyhow::{Result, ensure};
use orbment_core::i18n::tr;
use orbment_core::{config::MonitorAlerts, db};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use windows::Win32::System::SystemInformation::GetLocalTime;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct DailyTraffic {
    pub day_date: String,
    pub upload_bytes: u64,
    pub download_bytes: u64,
}

pub fn local_day() -> String {
    let now = unsafe { GetLocalTime() };
    format!("{:04}-{:02}-{:02}", now.wYear, now.wMonth, now.wDay)
}

fn read_day(connection: &Connection, day: &str) -> Result<DailyTraffic> {
    Ok(connection
        .query_row(
            "SELECT upload_bytes,download_bytes FROM traffic_daily_history WHERE day_date=?1",
            [day],
            |row| {
                Ok(DailyTraffic {
                    day_date: day.into(),
                    upload_bytes: row.get(0)?,
                    download_bytes: row.get(1)?,
                })
            },
        )
        .optional()?
        .unwrap_or(DailyTraffic {
            day_date: day.into(),
            ..Default::default()
        }))
}

pub struct TrafficHistory {
    connection: Connection,
    pending: BTreeMap<String, (u64, u64)>,
    today: DailyTraffic,
    adapter_mode: String,
}

impl TrafficHistory {
    pub fn open(data_dir: &Path) -> Result<Self> {
        let connection = db::open(data_dir)?;
        let day = local_day();
        let today = read_day(&connection, &day)?;
        Ok(Self {
            connection,
            pending: BTreeMap::new(),
            today,
            adapter_mode: "auto".into(),
        })
    }

    /// Account exact counter deltas, never rounded bytes-per-second values.
    pub fn accumulate(&mut self, day: &str, upload: u64, download: u64) -> Result<&DailyTraffic> {
        let pending = self.pending.entry(day.into()).or_default();
        pending.0 = pending.0.saturating_add(upload);
        pending.1 = pending.1.saturating_add(download);
        if self.today.day_date != day {
            // Retain deltas even if a transient SQLite read fails during midnight rollover.
            self.today = read_day(&self.connection, day)?;
            self.today.upload_bytes = self.today.upload_bytes.saturating_add(pending.0);
            self.today.download_bytes = self.today.download_bytes.saturating_add(pending.1);
        } else {
            self.today.upload_bytes = self.today.upload_bytes.saturating_add(upload);
            self.today.download_bytes = self.today.download_bytes.saturating_add(download);
        }
        Ok(&self.today)
    }

    pub fn today(&self) -> &DailyTraffic {
        &self.today
    }
    pub fn set_adapter_mode(&mut self, mode: String) {
        self.adapter_mode = mode;
    }

    pub fn flush(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let transaction = self.connection.transaction()?;
        for (day, (up, down)) in &self.pending {
            transaction.execute(
                "INSERT INTO traffic_daily_history(day_date,upload_bytes,download_bytes,adapter_mode,updated_at) \
                 VALUES (?1,?2,?3,?4,strftime('%Y-%m-%dT%H:%M:%fZ','now')) \
                 ON CONFLICT(day_date) DO UPDATE SET upload_bytes=upload_bytes+excluded.upload_bytes, \
                 download_bytes=download_bytes+excluded.download_bytes,adapter_mode=excluded.adapter_mode,updated_at=excluded.updated_at",
                params![day, i64::try_from(*up)?, i64::try_from(*down)?, self.adapter_mode],
            )?;
        }
        transaction.commit()?;
        self.pending.clear();
        Ok(())
    }

    pub fn query(&mut self, start: Option<&str>, end: Option<&str>) -> Result<Vec<DailyTraffic>> {
        self.flush()?;
        query(&self.connection, start, end)
    }
}

impl Drop for TrafficHistory {
    fn drop(&mut self) {
        if let Err(error) = self.flush() {
            tracing::error!(%error, "Flushing daily traffic history");
        }
    }
}

pub fn query(
    connection: &Connection,
    start: Option<&str>,
    end: Option<&str>,
) -> Result<Vec<DailyTraffic>> {
    for day in [start, end].into_iter().flatten() {
        validate_day(day)?;
    }
    if let (Some(start), Some(end)) = (start, end) {
        ensure!(start <= end, "{}", tr("monitor-history-invalid-range"));
    }
    let mut statement = connection.prepare(
        "SELECT day_date,upload_bytes,download_bytes FROM traffic_daily_history \
         WHERE (?1 IS NULL OR day_date>=?1) AND (?2 IS NULL OR day_date<=?2) ORDER BY day_date",
    )?;
    let rows = statement.query_map(params![start, end], |row| {
        Ok(DailyTraffic {
            day_date: row.get(0)?,
            upload_bytes: row.get(1)?,
            download_bytes: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn validate_day(day: &str) -> Result<()> {
    let bytes = day.as_bytes();
    ensure!(
        bytes.len() == 10
            && bytes[4] == b'-'
            && bytes[7] == b'-'
            && bytes
                .iter()
                .enumerate()
                .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit()),
        "{}",
        tr("monitor-history-invalid-date")
    );
    let year: u32 = day[..4].parse()?;
    let month: usize = day[5..7].parse()?;
    let date: u32 = day[8..].parse()?;
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    ensure!(
        year != 0 && (1..=12).contains(&month) && date != 0 && date <= days[month - 1],
        "{}",
        tr("monitor-history-invalid-date")
    );
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
pub struct MonitorAlert {
    pub key: &'static str,
    pub value: f64,
    pub threshold: f64,
}

#[derive(Default)]
pub struct AlertState {
    day: String,
    traffic_sent: bool,
    above: [bool; 4],
}

impl AlertState {
    /// Traffic warns once per local day; other thresholds re-arm below 95%.
    pub fn evaluate(
        &mut self,
        config: &MonitorAlerts,
        traffic: &DailyTraffic,
        memory: Option<f64>,
        cpu_temperature: Option<f64>,
        gpu_temperature: Option<f64>,
        disk_temperature: Option<f64>,
    ) -> Vec<MonitorAlert> {
        let mut alerts = Vec::new();
        if self.day != traffic.day_date {
            self.day.clone_from(&traffic.day_date);
            self.traffic_sent = false;
        }
        let threshold = config.traffic_value as f64
            * if config.traffic_unit == "GB" {
                1073741824.0
            } else {
                1048576.0
            };
        let total = traffic.upload_bytes.saturating_add(traffic.download_bytes) as f64;
        if config.traffic_enabled && !self.traffic_sent && total >= threshold {
            alerts.push(MonitorAlert {
                key: "monitor-alert-traffic",
                value: total,
                threshold,
            });
            self.traffic_sent = true;
        }
        let samples = [
            (
                config.memory_enabled,
                memory,
                f64::from(config.memory_pct),
                "monitor-alert-memory",
            ),
            (
                config.cpu_temperature_enabled,
                cpu_temperature,
                config.cpu_temperature_c,
                "monitor-alert-cpu-temperature",
            ),
            (
                config.gpu_temperature_enabled,
                gpu_temperature,
                config.gpu_temperature_c,
                "monitor-alert-gpu-temperature",
            ),
            (
                config.disk_temperature_enabled,
                disk_temperature,
                config.disk_temperature_c,
                "monitor-alert-disk-temperature",
            ),
        ];
        for (index, (enabled, value, threshold, key)) in samples.into_iter().enumerate() {
            if !enabled {
                self.above[index] = false;
                continue;
            }
            let Some(value) = value else {
                continue;
            };
            if !self.above[index] && value >= threshold {
                self.above[index] = true;
                alerts.push(MonitorAlert {
                    key,
                    value,
                    threshold,
                });
            } else if value < threshold * 0.95 {
                self.above[index] = false;
            }
        }
        alerts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_dates_and_ranges_are_real() {
        assert!(validate_day("2024-02-29").is_ok());
        for bad in [
            "2023-02-29",
            "2024-04-31",
            "2024-00-01",
            "0000-01-01",
            "2024-1-01",
            "é024-01-1",
        ] {
            assert!(validate_day(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn traffic_once_daily_and_temperature_hysteresis() {
        let config = MonitorAlerts {
            traffic_enabled: true,
            traffic_value: 1,
            memory_enabled: true,
            memory_pct: 80,
            ..Default::default()
        };
        let mut state = AlertState::default();
        let mut traffic = DailyTraffic {
            day_date: "2026-10-21".into(),
            upload_bytes: 2_000_000,
            download_bytes: 0,
        };
        assert_eq!(
            state
                .evaluate(&config, &traffic, Some(81.0), None, None, None)
                .len(),
            2
        );
        assert!(
            state
                .evaluate(&config, &traffic, Some(79.0), None, None, None)
                .is_empty()
        );
        assert!(
            state
                .evaluate(&config, &traffic, Some(81.0), None, None, None)
                .is_empty()
        );
        assert!(
            state
                .evaluate(&config, &traffic, Some(70.0), None, None, None)
                .is_empty()
        );
        assert_eq!(
            state
                .evaluate(&config, &traffic, Some(81.0), None, None, None)
                .len(),
            1
        );
        traffic.day_date = "2026-10-22".into();
        assert_eq!(
            state
                .evaluate(&config, &traffic, None, None, None, None)
                .len(),
            1
        );
    }

    #[test]
    fn local_days_flush_additively_without_duplicate_replay() {
        let mut connection = Connection::open_in_memory().unwrap();
        db::initialize(&mut connection).unwrap();
        let mut history = TrafficHistory {
            connection,
            pending: BTreeMap::new(),
            today: DailyTraffic::default(),
            adapter_mode: "all".into(),
        };
        history.accumulate("2026-10-21", 7, 11).unwrap();
        history.flush().unwrap();
        history.flush().unwrap();
        history.accumulate("2026-10-21", 3, 5).unwrap();
        history.accumulate("2026-10-22", 13, 17).unwrap();
        let rows = history.query(None, None).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].upload_bytes, rows[0].download_bytes), (10, 16));
        assert_eq!((rows[1].upload_bytes, rows[1].download_bytes), (13, 17));
        // A clock/time-zone change back to an already persisted local day preserves its totals.
        assert_eq!(
            history.accumulate("2026-10-21", 1, 2).unwrap().upload_bytes,
            11
        );
    }
}
