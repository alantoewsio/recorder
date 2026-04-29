//! Wiring helpers for one or more [`BusMixer`] instances (multi-bus graphs).

use crate::error::Result;
use crate::mixer::{bus_mixer_legs, BusMixer, BusMixerConfig, MixerInputSink};
use crate::traits::AudioSink;

/// Stable id for an input strip in a mixer graph (UI / persistence).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InputStripId(pub u32);

/// Stable id for a bus in a mixer graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BusId(pub u32);

/// Spawn one bus with `config.legs.len()` async inputs; returns one [`MixerInputSink`] per leg.
pub fn spawn_single_bus_mixer(
    capacity: usize,
    config: BusMixerConfig,
    out: Box<dyn AudioSink>,
) -> Result<(Vec<MixerInputSink>, BusMixer)> {
    let n = config.legs.len();
    let pairs = bus_mixer_legs(capacity, n);
    let (sinks, rxs): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();
    let mixer = BusMixer::spawn(config, rxs, out)?;
    Ok((sinks, mixer))
}

/// Owns multiple [`BusMixer`] handles; call [`MixerGraph::stop`] to join all workers.
pub struct MixerGraph {
    buses: Vec<BusMixer>,
}

impl MixerGraph {
/// Spawn independent buses in declaration order.
    pub fn spawn_from_bus_specs(
        capacity: usize,
        specs: Vec<(BusMixerConfig, Box<dyn AudioSink>)>,
    ) -> Result<(Self, Vec<Vec<MixerInputSink>>)> {
        let mut buses = Vec::with_capacity(specs.len());
        let mut all_legs = Vec::with_capacity(specs.len());
        for (cfg, sink) in specs {
            let (sinks, m) = spawn_single_bus_mixer(capacity, cfg, sink)?;
            buses.push(m);
            all_legs.push(sinks);
        }
        Ok((MixerGraph { buses }, all_legs))
    }

    pub fn stop(self) {
        for b in self.buses {
            b.stop();
        }
    }
}
