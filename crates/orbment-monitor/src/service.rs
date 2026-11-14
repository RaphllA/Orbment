use crate::{
    history::{AlertState, DailyTraffic, MonitorAlert, TrafficHistory, local_day},
    sampler::{self, MetricFormatter, NetworkState},
    sensors::{SensorDemand, SensorSnapshot, Sensors},
    skin::{METRICS, Skin},
};
use anyhow::{Context, Result};
use orbment_core::{
    config::{MetricLabels, MonitorConfig},
    i18n::tr,
};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, RwLock,
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct MonitorSnapshot {
    pub upload_bytes_per_second: Option<f64>,
    pub download_bytes_per_second: Option<f64>,
    #[serde(flatten)]
    pub sensors: SensorSnapshot,
    pub today: DailyTraffic,
}

#[derive(Clone, Debug)]
pub struct MonitorItem {
    pub display_bit: u32,
    pub label: String,
    pub value: String,
    pub usage_fraction: Option<f32>,
}

#[derive(Clone, Debug)]
pub struct MonitorUpdate {
    pub snapshot: MonitorSnapshot,
    pub taskbar_items: Vec<MonitorItem>,
    pub floating_values: BTreeMap<String, String>,
    pub alerts: Vec<MonitorAlert>,
}

enum Command {
    Stop,
    Configure(Box<MonitorConfig>),
    History {
        start: Option<String>,
        end: Option<String>,
        reply: Box<dyn FnOnce(Result<Vec<DailyTraffic>>) + Send>,
    },
}

pub struct MonitorService {
    commands: Sender<Command>,
    snapshot: Arc<RwLock<MonitorSnapshot>>,
    worker: Option<JoinHandle<()>>,
}

impl MonitorService {
    pub fn start(
        config: MonitorConfig,
        data_dir: PathBuf,
        notify: impl Fn(MonitorUpdate) + Send + 'static,
    ) -> Result<Self> {
        // Open before spawning so database failures reach the initiating action synchronously.
        let history = TrafficHistory::open(&data_dir)?;
        let snapshot = Arc::new(RwLock::new(MonitorSnapshot {
            today: history.today().clone(),
            ..Default::default()
        }));
        let shared = Arc::clone(&snapshot);
        let (commands, receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("orbment-monitor-service".into())
            .spawn(move || run(config, data_dir, history, shared, receiver, notify))
            .context(tr("monitor-worker-start"))?;
        Ok(Self {
            commands,
            snapshot,
            worker: Some(worker),
        })
    }

    pub fn snapshot(&self) -> MonitorSnapshot {
        self.snapshot
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub fn update_config(&self, config: MonitorConfig) -> Result<()> {
        self.commands
            .send(Command::Configure(Box::new(config)))
            .map_err(|_| anyhow::anyhow!(tr("monitor-worker-stopped")))
    }

    /// Queries on the sampling worker; a stopped worker completes immediately with an error.
    pub fn traffic_history_async(
        &self,
        start: Option<String>,
        end: Option<String>,
        completion: impl FnOnce(Result<Vec<DailyTraffic>>) + Send + 'static,
    ) {
        if let Err(error) = self.commands.send(Command::History {
            start,
            end,
            reply: Box::new(completion),
        }) && let Command::History { reply, .. } = error.0
        {
            reply(Err(anyhow::anyhow!(tr("monitor-worker-stopped"))));
        }
    }
}

impl Drop for MonitorService {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::error!("Monitor service worker panicked");
        }
    }
}

fn sensor_demand(config: &MonitorConfig, data_dir: &std::path::Path) -> SensorDemand {
    let skin_mask = if config.floating.enabled {
        let skin = if config.floating.skin_selected.is_empty() {
            Skin::default()
        } else {
            match Skin::load(&data_dir.join("skins").join(&config.floating.skin_selected)) {
                Ok(skin) => skin,
                Err(error) => {
                    tracing::warn!(%error, "Monitor skin sensor demand");
                    Skin::default()
                }
            }
        };
        skin.display_mask(config.floating.show_cpu_memory)
    } else {
        0
    };
    let mut demand = SensorDemand::from_config(config, skin_mask);
    // Snapshot API always has the lightweight baseline CPU/memory measurements.
    demand.memory = true;
    demand.cpu_pdh = !config.cpu_usage_by_times;
    demand
}

fn create_sensors(
    config: &MonitorConfig,
    demand: SensorDemand,
    data_dir: &std::path::Path,
) -> Sensors {
    let module_dir = if config.pawnio_module_dir.is_empty() {
        data_dir.join("PawnIO")
    } else {
        PathBuf::from(&config.pawnio_module_dir)
    };
    Sensors::new(config, demand, &module_dir)
}

fn set_adapter_mode(history: &mut TrafficHistory, config: &MonitorConfig) {
    history.set_adapter_mode(if config.adapter_select_all {
        "all".into()
    } else if config.adapter_auto_select {
        "auto".into()
    } else {
        config.selected_adapter_name.clone()
    });
}

fn run(
    mut config: MonitorConfig,
    data_dir: PathBuf,
    mut history: TrafficHistory,
    shared: Arc<RwLock<MonitorSnapshot>>,
    receiver: Receiver<Command>,
    notify: impl Fn(MonitorUpdate),
) {
    let mut demand = sensor_demand(&config, &data_dir);
    let mut sensors = create_sensors(&config, demand, &data_dir);
    set_adapter_mode(&mut history, &config);
    let mut network = NetworkState::default();
    let mut previous_cpu = None;
    let mut alerts = AlertState::default();
    let formatter = MetricFormatter::new();
    let mut interval = Duration::from_millis(u64::from(config.sample_interval_ms));
    let mut next_flush = Instant::now() + Duration::from_secs(30);
    loop {
        let started = Instant::now();
        let rates = match sampler::read_interfaces() {
            Ok(interfaces) => Some(network.update(interfaces, started, &config)),
            Err(error) => {
                tracing::warn!(%error, "Reading network counters");
                network = NetworkState::default();
                None
            }
        };
        let mut hardware = sensors.sample();
        if config.cpu_usage_by_times {
            match sampler::read_cpu_times() {
                Ok(current) => {
                    hardware.cpu_percent = Some(sampler::cpu_usage(previous_cpu, current));
                    previous_cpu = Some(current);
                }
                Err(error) => {
                    tracing::warn!(%error, "Reading CPU counters");
                    previous_cpu = None;
                }
            }
        }
        if let Err(error) = history.accumulate(
            &local_day(),
            network.last_delta.sent,
            network.last_delta.received,
        ) {
            tracing::error!(%error, "Accumulating local daily traffic");
        }
        if started >= next_flush {
            if let Err(error) = history.flush() {
                tracing::error!(%error, "Persisting daily traffic");
            }
            next_flush = started + Duration::from_secs(30);
        }
        let snapshot = MonitorSnapshot {
            upload_bytes_per_second: rates.map(|r| r.up),
            download_bytes_per_second: rates.map(|r| r.down),
            sensors: hardware,
            today: history.today().clone(),
        };
        let notifications = alerts.evaluate(
            &config.alerts,
            &snapshot.today,
            snapshot.sensors.memory_percent,
            snapshot.sensors.cpu_temperature_c,
            snapshot.sensors.gpu_temperature_c,
            snapshot.sensors.disk_temperature_c,
        );
        *shared.write().unwrap_or_else(|poison| poison.into_inner()) = snapshot.clone();
        if config.taskbar.enabled || config.floating.enabled || !notifications.is_empty() {
            let values = values(&config, &snapshot, &formatter, false);
            let mut taskbar_format = config.clone();
            taskbar_format.unit_byte = config.taskbar.unit_byte;
            taskbar_format.speed_unit = config.taskbar.speed_unit;
            taskbar_format.hide_unit = config.taskbar.hide_unit;
            taskbar_format.hide_percent = config.taskbar.hide_percent;
            taskbar_format.speed_short_mode = config.taskbar.speed_short_mode;
            taskbar_format.separate_value_unit_with_space =
                config.taskbar.separate_value_unit_with_space;
            taskbar_format.memory_display_mode = config.taskbar.memory_display_mode;
            let taskbar_values = values_for_taskbar(&taskbar_format, &snapshot, &formatter);
            let mut taskbar_items: Vec<_> = METRICS
                .iter()
                .filter(|(_, bit)| config.taskbar.display_mask & bit != 0)
                .map(|(key, bit)| MonitorItem {
                    display_bit: *bit,
                    label: label(&config.taskbar.labels, key).into(),
                    value: taskbar_values.get(*key).cloned().unwrap_or_default(),
                    usage_fraction: match *key {
                        "cpu" => snapshot.sensors.cpu_percent,
                        "memory" => snapshot.sensors.memory_percent,
                        "gpu" => snapshot.sensors.gpu_percent,
                        "disk" => snapshot.sensors.disk_percent,
                        _ => None,
                    }
                    .map(|percent| (percent / 100.0) as f32),
                })
                .collect();
            taskbar_items.sort_by_key(|item| {
                config
                    .taskbar
                    .item_order
                    .iter()
                    .position(|index| u32::from(*index) == item.display_bit.trailing_zeros())
                    .unwrap_or(usize::MAX)
            });
            notify(MonitorUpdate {
                snapshot,
                taskbar_items,
                floating_values: values,
                alerts: notifications,
            });
        }
        let mut deadline = started + interval;
        loop {
            match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Command::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Ok(Command::History { start, end, reply }) => {
                    reply(history.query(start.as_deref(), end.as_deref()));
                }
                Ok(Command::Configure(updated)) => {
                    let next_demand = sensor_demand(&updated, &data_dir);
                    if demand != next_demand
                        || config.cpu_core_name != updated.cpu_core_name
                        || config.hard_disk_name != updated.hard_disk_name
                        || config.pawnio_module_dir != updated.pawnio_module_dir
                    {
                        sensors = create_sensors(&updated, next_demand, &data_dir);
                        demand = next_demand;
                    }
                    if config.cpu_usage_by_times != updated.cpu_usage_by_times {
                        previous_cpu = None;
                    }
                    set_adapter_mode(&mut history, &updated);
                    interval = Duration::from_millis(u64::from(updated.sample_interval_ms));
                    config = *updated;
                    deadline = started + interval;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => break,
            }
        }
    }
}

pub(crate) fn label<'a>(labels: &'a MetricLabels, key: &str) -> &'a str {
    match key {
        "up" => &labels.up,
        "down" => &labels.down,
        "cpu" => &labels.cpu,
        "memory" => &labels.memory,
        "gpu" => &labels.gpu,
        "disk" => &labels.disk,
        "cpu_temperature" => &labels.cpu_temperature,
        "gpu_temperature" => &labels.gpu_temperature,
        "disk_temperature" => &labels.disk_temperature,
        "cpu_frequency" => &labels.cpu_frequency,
        "today_traffic" => &labels.today_traffic,
        "disk_read" => &labels.disk_read,
        "disk_write" => &labels.disk_write,
        "total_speed" => &labels.total_speed,
        _ => "",
    }
}

fn values_for_taskbar(
    config: &MonitorConfig,
    snapshot: &MonitorSnapshot,
    formatter: &MetricFormatter,
) -> BTreeMap<String, String> {
    let mut result = values(config, snapshot, formatter, true);
    // Reserve the requested numeric width independently of the localized speed unit.
    for key in ["up", "down", "total_speed", "disk_read", "disk_write"] {
        if let Some(value) = result.get_mut(key) {
            let end = value
                .find(|character: char| !character.is_ascii_digit() && character != '.')
                .unwrap_or(value.len());
            if end != 0 {
                let (number, unit) = value.split_at(end);
                let width = usize::from(config.taskbar.digits_number.clamp(1, 20));
                *value = format!("{number:>width$}{unit}");
            }
        }
    }
    result
}

fn values(
    config: &MonitorConfig,
    snapshot: &MonitorSnapshot,
    formatter: &MetricFormatter,
    separate_scalar_units: bool,
) -> BTreeMap<String, String> {
    let sensor = &snapshot.sensors;
    let unavailable = tr("monitor-unavailable");
    let percent = if config.hide_percent { "" } else { "%" };
    let temperature = if config.hide_unit {
        String::new()
    } else {
        tr("monitor-unit-celsius")
    };
    let frequency = if config.hide_unit {
        String::new()
    } else {
        tr("monitor-unit-mhz")
    };
    let mut result = BTreeMap::new();
    for (key, value) in [
        ("up", snapshot.upload_bytes_per_second),
        ("down", snapshot.download_bytes_per_second),
        (
            "total_speed",
            snapshot
                .upload_bytes_per_second
                .zip(snapshot.download_bytes_per_second)
                .map(|(up, down)| up + down),
        ),
        ("disk_read", sensor.disk_read_bytes_per_second),
        ("disk_write", sensor.disk_write_bytes_per_second),
    ] {
        result.insert(
            key.into(),
            value.map_or_else(
                || unavailable.clone(),
                |value| formatter.speed(config, value),
            ),
        );
    }
    for (key, value, unit) in [
        ("cpu", sensor.cpu_percent, percent),
        ("memory", sensor.memory_percent, percent),
        ("gpu", sensor.gpu_percent, percent),
        ("disk", sensor.disk_percent, percent),
        ("cpu_temperature", sensor.cpu_temperature_c, &temperature),
        ("gpu_temperature", sensor.gpu_temperature_c, &temperature),
        ("disk_temperature", sensor.disk_temperature_c, &temperature),
        ("cpu_frequency", sensor.cpu_frequency_mhz, &frequency),
    ] {
        result.insert(
            key.into(),
            value.map_or_else(
                || unavailable.clone(),
                |value| {
                    let space = if separate_scalar_units
                        && config.separate_value_unit_with_space
                        && !unit.is_empty()
                    {
                        " "
                    } else {
                        ""
                    };
                    format!("{value:.0}{space}{unit}")
                },
            ),
        );
    }
    let mut byte_config = config.clone();
    byte_config.unit_byte = true;
    byte_config.speed_short_mode = true;
    byte_config.speed_unit = 0;
    result.insert(
        "today_traffic".into(),
        formatter.speed(
            &byte_config,
            snapshot
                .today
                .upload_bytes
                .saturating_add(snapshot.today.download_bytes) as f64,
        ),
    );
    if config.memory_display_mode != 0 {
        let bytes = if config.memory_display_mode == 1 {
            sensor
                .memory_total_bytes
                .zip(sensor.memory_available_bytes)
                .map(|(total, available)| total.saturating_sub(available))
        } else {
            sensor.memory_available_bytes
        };
        result.insert(
            "memory".into(),
            bytes.map_or_else(
                || unavailable.clone(),
                |bytes| formatter.speed(&byte_config, bytes as f64),
            ),
        );
    }
    // CPU temperature carries the actionable driver/module status directly into the display.
    if let Some(key) = sensor.unavailable.get("cpu_temperature") {
        result.insert("cpu_temperature".into(), tr(key));
    }
    result
}
