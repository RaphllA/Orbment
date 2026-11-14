use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use orbment_core::config::MonitorConfig;
use orbment_core::i18n::tr;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, IF_TYPE_SOFTWARE_LOOPBACK, MIB_IF_ROW2, MIB_IF_TABLE2,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::System::Threading::GetSystemTimes;

#[derive(Clone, Copy, Default)]
pub(crate) struct Counters {
    pub(crate) received: u64,
    pub(crate) sent: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Rates {
    pub(crate) down: f64,
    pub(crate) up: f64,
}

fn counter_rates(previous: Option<Counters>, current: Counters, elapsed: Duration) -> Rates {
    let Some(previous) = previous else {
        return Rates::default();
    };
    // Counter rollback marks a new interface epoch, not unsigned wraparound traffic.
    let (Some(received), Some(sent)) = (
        current.received.checked_sub(previous.received),
        current.sent.checked_sub(previous.sent),
    ) else {
        return Rates::default();
    };
    if elapsed.is_zero() {
        return Rates::default();
    }
    Rates {
        down: received as f64 / elapsed.as_secs_f64(),
        up: sent as f64 / elapsed.as_secs_f64(),
    }
}

pub(crate) struct InterfaceSample {
    id: u64,
    alias: String,
    description: String,
    counters: Counters,
}

impl InterfaceSample {
    fn matches_name(&self, name: &str) -> bool {
        self.alias.eq_ignore_ascii_case(name) || self.description.eq_ignore_ascii_case(name)
    }
}

#[derive(Default)]
pub(crate) struct NetworkState {
    previous: BTreeMap<u64, Counters>,
    sampled_at: Option<Instant>,
    selected: Option<u64>,
    pub(crate) last_delta: Counters,
}

impl NetworkState {
    pub(crate) fn update(
        &mut self,
        interfaces: Vec<InterfaceSample>,
        now: Instant,
        config: &MonitorConfig,
    ) -> Rates {
        let elapsed = self
            .sampled_at
            .map_or(Duration::ZERO, |previous| now.duration_since(previous));
        let mut next = BTreeMap::new();
        let mut total = Rates::default();
        let mut best: Option<(u64, Rates)> = None;
        for interface in interfaces {
            if config
                .hidden_adapters
                .iter()
                .any(|name| interface.matches_name(name))
            {
                continue;
            }
            let rates = counter_rates(
                self.previous.get(&interface.id).copied(),
                interface.counters,
                elapsed,
            );
            next.insert(interface.id, interface.counters);
            if config.adapter_select_all {
                total.down += rates.down;
                total.up += rates.up;
            } else if config.adapter_auto_select {
                let activity = rates.down + rates.up;
                let replace = match best {
                    None => true,
                    Some((_, best_rates)) => {
                        activity > best_rates.down + best_rates.up
                            || (activity == best_rates.down + best_rates.up
                                && self.selected == Some(interface.id))
                    }
                };
                if replace {
                    best = Some((interface.id, rates));
                }
            } else if interface.matches_name(&config.selected_adapter_name) {
                // Alias names are unique; description matching also accepts imported configurations.
                if best.is_none()
                    || interface
                        .alias
                        .eq_ignore_ascii_case(&config.selected_adapter_name)
                {
                    best = Some((interface.id, rates));
                }
            }
        }
        self.last_delta = Counters::default();
        for (id, current) in &next {
            if !config.adapter_select_all && best.as_ref().map(|(selected, _)| selected) != Some(id)
            {
                continue;
            }
            if let Some(previous) = self.previous.get(id)
                && let (Some(received), Some(sent)) = (
                    current.received.checked_sub(previous.received),
                    current.sent.checked_sub(previous.sent),
                )
            {
                self.last_delta.received = self.last_delta.received.saturating_add(received);
                self.last_delta.sent = self.last_delta.sent.saturating_add(sent);
            }
        }
        self.previous = next;
        self.sampled_at = Some(now);
        self.selected = best.map(|(id, _)| id);
        if config.adapter_select_all {
            total
        } else {
            best.map_or_else(Rates::default, |(_, rates)| rates)
        }
    }
}

struct InterfaceTable(*mut MIB_IF_TABLE2);

impl Drop for InterfaceTable {
    fn drop(&mut self) {
        unsafe { FreeMibTable(self.0.cast()) };
    }
}

pub(crate) fn read_interfaces() -> Result<Vec<InterfaceSample>> {
    let mut table = std::ptr::null_mut();
    let status = unsafe { GetIfTable2(&mut table) };
    ensure!(
        status.0 == 0,
        "GetIfTable2 failed with Win32 error {}",
        status.0
    );
    let table = InterfaceTable(table);
    // GetIfTable2 owns a variable-length allocation; Table's declared length is only one.
    let rows = unsafe {
        std::slice::from_raw_parts(
            std::ptr::addr_of!((*table.0).Table).cast::<MIB_IF_ROW2>(),
            (*table.0).NumEntries as usize,
        )
    };
    Ok(rows
        .iter()
        .filter(|row| row.Type != IF_TYPE_SOFTWARE_LOOPBACK && row.OperStatus == IfOperStatusUp)
        .map(|row| InterfaceSample {
            id: unsafe { row.InterfaceLuid.Value },
            alias: decode_name(&row.Alias),
            description: decode_name(&row.Description),
            counters: Counters {
                received: row.InOctets,
                sent: row.OutOctets,
            },
        })
        .collect())
}

fn decode_name(value: &[u16]) -> String {
    let length = value
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(value.len());
    String::from_utf16_lossy(&value[..length])
}

#[derive(Clone, Copy)]
pub(crate) struct CpuTimes {
    idle: u64,
    kernel: u64,
    user: u64,
}

pub(crate) fn cpu_usage(previous: Option<CpuTimes>, current: CpuTimes) -> f64 {
    let Some(previous) = previous else {
        return 0.0;
    };
    let (Some(idle), Some(kernel), Some(user)) = (
        current.idle.checked_sub(previous.idle),
        current.kernel.checked_sub(previous.kernel),
        current.user.checked_sub(previous.user),
    ) else {
        return 0.0;
    };
    // Kernel time includes idle time; adding idle again would understate utilization.
    let total = u128::from(kernel) + u128::from(user);
    if total == 0 {
        return 0.0;
    }
    100.0 * total.saturating_sub(u128::from(idle)) as f64 / total as f64
}

pub(crate) fn read_cpu_times() -> Result<CpuTimes> {
    let mut idle = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }
        .context("GetSystemTimes")?;
    let ticks =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    Ok(CpuTimes {
        idle: ticks(idle),
        kernel: ticks(kernel),
        user: ticks(user),
    })
}

pub(crate) struct MetricFormatter {
    byte: String,
    bit: String,
    per_second: String,
}

impl MetricFormatter {
    pub(crate) fn new() -> Self {
        Self {
            byte: tr("monitor-unit-byte"),
            bit: tr("monitor-unit-bit"),
            per_second: tr("monitor-unit-per-second"),
        }
    }

    pub(crate) fn speed(&self, config: &MonitorConfig, bytes_per_second: f64) -> String {
        const PREFIXES: [&str; 7] = ["", "K", "M", "G", "T", "P", "E"];
        let mut value = bytes_per_second * if config.unit_byte { 1.0 } else { 8.0 };
        let mut scale = usize::from(config.speed_unit).min(PREFIXES.len() - 1);
        if scale == 0 {
            while value >= 1024.0 && scale < PREFIXES.len() - 1 {
                value /= 1024.0;
                scale += 1;
            }
        } else {
            value /= 1024.0_f64.powi(scale as i32);
        }
        let precision = if value < 10.0 {
            2
        } else if value < 100.0 {
            1
        } else {
            0
        };
        let number = format!("{value:.precision$}");
        if config.hide_unit {
            return number;
        }
        let space = if config.separate_value_unit_with_space {
            " "
        } else {
            ""
        };
        let unit = if config.unit_byte {
            &self.byte
        } else {
            &self.bit
        };
        let suffix = if config.speed_short_mode {
            ""
        } else {
            &self.per_second
        };
        format!("{number}{space}{}{unit}{suffix}", PREFIXES[scale])
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Counters, CpuTimes, InterfaceSample, NetworkState, Rates, counter_rates, cpu_usage,
    };
    use orbment_core::config::MonitorConfig;
    use std::time::{Duration, Instant};

    #[test]
    fn network_rates_use_actual_elapsed_time_and_64_bit_deltas() {
        let previous = Counters {
            received: 1 << 40,
            sent: 1 << 42,
        };
        let current = Counters {
            received: previous.received + 3000,
            sent: previous.sent + 900,
        };
        assert_eq!(
            counter_rates(Some(previous), current, Duration::from_millis(1500)),
            Rates {
                down: 2000.0,
                up: 600.0
            },
        );
    }

    #[test]
    fn first_zero_interval_and_reset_network_samples_are_zero() {
        let previous = Counters {
            received: 1000,
            sent: 2000,
        };
        let current = Counters {
            received: 5000,
            sent: 6000,
        };
        let second = Duration::from_secs(1);
        assert_eq!(counter_rates(None, current, second), Rates::default());
        assert_eq!(
            counter_rates(Some(previous), current, Duration::ZERO),
            Rates::default()
        );
        for reset in [
            Counters {
                received: 1,
                sent: 6000,
            },
            Counters {
                received: 5000,
                sent: 1,
            },
        ] {
            assert_eq!(
                counter_rates(Some(previous), reset, second),
                Rates::default()
            );
            let next = Counters {
                received: reset.received + 500,
                sent: reset.sent + 250,
            };
            assert_eq!(
                counter_rates(Some(reset), next, second),
                Rates {
                    down: 500.0,
                    up: 250.0
                }
            );
        }
    }

    fn interface(id: u64, received: u64, sent: u64) -> InterfaceSample {
        InterfaceSample {
            id,
            alias: format!("adapter-{id}"),
            description: format!("description-{id}"),
            counters: Counters { received, sent },
        }
    }

    #[test]
    fn hidden_adapters_are_excluded_from_all_selection_modes() {
        for (select_all, auto_select, expected_down) in [
            (true, false, 100.0),
            (false, true, 100.0),
            (false, false, 0.0),
        ] {
            let config = MonitorConfig {
                adapter_select_all: select_all,
                adapter_auto_select: auto_select,
                selected_adapter_name: "adapter-1".into(),
                hidden_adapters: vec!["ADAPTER-1".into(), "description-2".into()],
                ..Default::default()
            };
            let now = Instant::now();
            let mut state = NetworkState::default();
            state.update(
                vec![interface(1, 0, 0), interface(2, 0, 0), interface(3, 0, 0)],
                now,
                &config,
            );
            let rates = state.update(
                vec![
                    interface(1, 1000, 0),
                    interface(2, 2000, 0),
                    interface(3, 100, 0),
                ],
                now + Duration::from_secs(1),
                &config,
            );
            assert_eq!(rates.down, expected_down);
        }
    }

    #[test]
    fn aggregate_rates_ignore_new_and_disappearing_interface_totals() {
        let config = MonitorConfig {
            adapter_select_all: true,
            ..Default::default()
        };
        let mut state = NetworkState::default();
        let now = Instant::now();
        assert_eq!(
            state.update(
                vec![interface(1, 1000, 1000), interface(2, 9000, 9000)],
                now,
                &config
            ),
            Rates::default(),
        );
        assert_eq!(
            state.update(
                vec![interface(1, 1500, 1200), interface(3, 1 << 40, 1 << 40)],
                now + Duration::from_secs(1),
                &config,
            ),
            Rates {
                down: 500.0,
                up: 200.0
            },
        );
        assert_eq!(
            state.update(
                vec![interface(1, 2000, 1500), interface(2, 9999, 9999)],
                now + Duration::from_secs(2),
                &config,
            ),
            Rates {
                down: 500.0,
                up: 300.0
            },
        );
    }

    #[test]
    fn automatic_handover_uses_each_adapters_own_counter_baseline() {
        let config = MonitorConfig {
            adapter_select_all: false,
            adapter_auto_select: true,
            ..Default::default()
        };
        let mut state = NetworkState::default();
        let now = Instant::now();
        state.update(
            vec![interface(1, 1000, 1000), interface(2, 100_000, 100_000)],
            now,
            &config,
        );
        assert_eq!(
            state.update(
                vec![interface(1, 2000, 2000), interface(2, 100_010, 100_020)],
                now + Duration::from_secs(1),
                &config,
            ),
            Rates {
                down: 1000.0,
                up: 1000.0
            },
        );
        assert_eq!(
            state.update(
                vec![interface(1, 2100, 2100), interface(2, 102_010, 103_020)],
                now + Duration::from_secs(2),
                &config,
            ),
            Rates {
                down: 2000.0,
                up: 3000.0
            },
        );
    }

    #[test]
    fn cpu_delta_counts_idle_as_part_of_kernel() {
        let previous = CpuTimes {
            idle: 100,
            kernel: 200,
            user: 300,
        };
        let current = CpuTimes {
            idle: 150,
            kernel: 300,
            user: 400,
        };
        assert_eq!(cpu_usage(Some(previous), current), 75.0);
        assert_eq!(cpu_usage(None, current), 0.0);
        assert_eq!(cpu_usage(Some(current), current), 0.0);
    }

    #[test]
    fn cpu_reset_and_inconsistent_idle_do_not_overflow() {
        let previous = CpuTimes {
            idle: 100,
            kernel: 200,
            user: 300,
        };
        for reset in [
            CpuTimes {
                idle: 99,
                kernel: 300,
                user: 400,
            },
            CpuTimes {
                idle: 150,
                kernel: 199,
                user: 400,
            },
            CpuTimes {
                idle: 150,
                kernel: 300,
                user: 299,
            },
            CpuTimes {
                idle: 500,
                kernel: 300,
                user: 400,
            },
        ] {
            assert_eq!(cpu_usage(Some(previous), reset), 0.0);
        }
    }
}
