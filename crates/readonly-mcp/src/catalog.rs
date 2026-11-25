//! In-memory **synthetic** dataset. Every site, machine, tag and number here is invented.

use serde::Serialize;

/// Operating state of a machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineState {
    /// Producing.
    Running,
    /// Powered, not producing.
    Idle,
    /// Stopped by fault or operator.
    Stopped,
    /// Planned maintenance.
    Maintenance,
}

/// One tag reading, e.g. `filler.speed`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TagReading {
    /// Dotted tag name.
    pub name: String,
    /// Last value.
    pub value: f64,
    /// Engineering unit.
    pub unit: String,
}

/// Raw shift counters from which OEE is derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ShiftCounters {
    /// Planned production time, seconds.
    pub planned_time_s: u64,
    /// Actual run time, seconds.
    pub run_time_s: u64,
    /// Ideal (rated) time per unit, milliseconds.
    pub ideal_cycle_time_ms: u64,
    /// Units produced.
    pub total_count: u64,
    /// Units passing quality.
    pub good_count: u64,
}

/// A synthetic machine.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Machine {
    /// Machine id, e.g. `line-a1`.
    pub id: String,
    /// Owning site id, e.g. `site-alpha`.
    pub site: String,
    /// Human-readable name.
    pub display_name: String,
    /// Current state.
    pub state: MachineState,
    /// Latest tag readings.
    pub tags: Vec<TagReading>,
    /// Counters for the current synthetic shift.
    pub counters: ShiftCounters,
}

/// Immutable dataset shared (by `&` reference) with every session.
#[derive(Debug, Clone, PartialEq)]
pub struct Catalog {
    machines: Vec<Machine>,
}

fn tag(name: &str, value: f64, unit: &str) -> TagReading {
    TagReading {
        name: name.to_owned(),
        value,
        unit: unit.to_owned(),
    }
}

/// Private table-row builder; `counters` is
/// `[planned_s, run_s, ideal_cycle_ms, total, good]`.
fn machine(
    id: &str,
    site: &str,
    display_name: &str,
    state: MachineState,
    speed: f64,
    counters: [u64; 5],
) -> Machine {
    let [planned_time_s, run_time_s, ideal_cycle_time_ms, total_count, good_count] = counters;
    Machine {
        id: id.to_owned(),
        site: site.to_owned(),
        display_name: display_name.to_owned(),
        state,
        tags: vec![
            tag("filler.speed", speed, "units/min"),
            tag("capper.torque", 12.5, "N*cm"),
            tag("labeler.reject_count", 3.0, "count"),
        ],
        counters: ShiftCounters {
            planned_time_s,
            run_time_s,
            ideal_cycle_time_ms,
            total_count,
            good_count,
        },
    }
}

impl Catalog {
    /// The built-in synthetic dataset: three sites, five machines.
    #[must_use]
    pub fn synthetic() -> Self {
        use MachineState::{Idle, Maintenance, Running};
        Self {
            machines: vec![
                machine(
                    "line-a1",
                    "site-alpha",
                    "Line A1",
                    Running,
                    57.0,
                    [28_800, 25_920, 1_000, 24_624, 24_131],
                ),
                machine(
                    "line-a2",
                    "site-alpha",
                    "Line A2",
                    Idle,
                    0.0,
                    [28_800, 14_400, 1_500, 8_640, 8_467],
                ),
                machine(
                    "line-b1",
                    "site-beta",
                    "Line B1",
                    Running,
                    27.0,
                    [3_600, 3_000, 2_000, 1_350, 1_215],
                ),
                machine(
                    "line-b2",
                    "site-beta",
                    "Line B2",
                    Maintenance,
                    0.0,
                    [28_800, 0, 1_200, 0, 0],
                ),
                machine(
                    "line-c1",
                    "site-gamma",
                    "Line C1",
                    Running,
                    70.0,
                    [28_800, 27_000, 800, 31_500, 31_185],
                ),
            ],
        }
    }

    /// Build a catalog from arbitrary machines (used by tests).
    #[must_use]
    pub fn from_machines(machines: Vec<Machine>) -> Self {
        Self { machines }
    }

    /// All machines, in catalog order.
    #[must_use]
    pub fn machines(&self) -> &[Machine] {
        &self.machines
    }

    /// Look up a machine by id.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Machine> {
        self.machines.iter().find(|m| m.id == id)
    }

    /// Site that owns machine `id`.
    #[must_use]
    pub fn site_of(&self, id: &str) -> Option<&str> {
        self.get(id).map(|m| m.site.as_str())
    }
}
