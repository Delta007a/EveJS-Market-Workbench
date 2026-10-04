//! NPC eligibility is a join of matching-build CCP SDE and generated static data.
//! IDs alone, live structures and unknown owners are never eligibility authorities.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::CANONICAL_HUB_STATION_IDS;
use crate::policy_preview::sha256_hex;
use crate::staticdata::StaticData;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NpcStation {
    pub station_id: u64,
    pub name: String,
    pub owner_id: u32,
    pub solar_system_id: u32,
    pub constellation_id: u32,
    pub region_id: u32,
    pub region_name: String,
    pub nearest_hub_jumps: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct Inventory {
    pub stations: BTreeMap<u64, NpcStation>,
    pub provenance: Value,
    pub topology_available: bool,
}

#[derive(Deserialize)]
struct RawStation {
    #[serde(rename = "_key")]
    id: u64,
    #[serde(rename = "ownerID")]
    owner: u32,
    #[serde(rename = "solarSystemID")]
    system: u32,
    #[serde(rename = "typeID")]
    type_id: u32,
}
#[derive(Deserialize)]
struct RawCorp {
    #[serde(rename = "_key")]
    id: u32,
    #[serde(default)]
    deleted: bool,
}
#[derive(Deserialize)]
struct Gate {
    #[serde(rename = "_key")]
    id: u64,
    #[serde(rename = "solarSystemID")]
    system: u32,
    destination: Destination,
}
#[derive(Deserialize)]
struct Destination {
    #[serde(rename = "solarSystemID")]
    system: u32,
    #[serde(rename = "stargateID")]
    gate: u64,
}

fn jsonl<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    BufReader::new(fs::File::open(path)?)
        .lines()
        .filter_map(|l| match l {
            Ok(s) if s.trim().is_empty() => None,
            v => Some(
                v.map_err(anyhow::Error::from)
                    .and_then(|s| Ok(serde_json::from_str(&s)?)),
            ),
        })
        .collect()
}

pub fn jump_distances(
    adjacency: &BTreeMap<u32, BTreeSet<u32>>,
    roots: &[u32],
) -> BTreeMap<u32, u32> {
    let mut distances = BTreeMap::new();
    let mut queue = VecDeque::new();
    for &root in roots {
        if distances.insert(root, 0).is_none() {
            queue.push_back(root);
        }
    }
    while let Some(system) = queue.pop_front() {
        let next = distances[&system] + 1;
        for &neighbor in adjacency.get(&system).into_iter().flatten() {
            if let std::collections::btree_map::Entry::Vacant(entry) = distances.entry(neighbor) {
                entry.insert(next);
                queue.push_back(neighbor);
            }
        }
    }
    distances
}

impl Inventory {
    pub fn load(data: &StaticData, build: u64) -> Result<Self> {
        let local = data
            .dir
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| anyhow!("static data has no adjacent SDE"))?;
        let raw = local
            .join("sde")
            .join(format!("eve-online-static-data-{build}-jsonl"));
        let header: Vec<Value> = jsonl(&raw.join("_sde.jsonl"))?;
        ensure!(
            header.first().and_then(|h| h["buildNumber"].as_u64()) == Some(build),
            "NPC-station SDE build mismatch"
        );
        let owners: BTreeSet<_> = jsonl::<RawCorp>(&raw.join("npcCorporations.jsonl"))?
            .into_iter()
            .filter(|c| !c.deleted)
            .map(|c| c.id)
            .collect();
        let mut raw_stations = BTreeMap::new();
        for s in jsonl::<RawStation>(&raw.join("npcStations.jsonl"))? {
            ensure!(
                raw_stations.insert(s.id, s).is_none(),
                "duplicate SDE NPC station"
            );
        }
        let generated: Value =
            serde_json::from_slice(&fs::read(data.dir.join("stations/data.json"))?)?;
        let mut stations = BTreeMap::new();
        for row in generated["stations"]
            .as_array()
            .ok_or_else(|| anyhow!("generated stations missing"))?
        {
            let Some(id) = row["stationID"].as_u64() else {
                continue;
            };
            let Some(raw_station) = raw_stations.get(&id) else {
                continue;
            };
            if !owners.contains(&raw_station.owner) {
                continue;
            }
            // Also reject forged generated metadata, player station types, and mismatched geometry.
            ensure!(
                row["corporationID"].as_u64() == Some(raw_station.owner as u64),
                "station {id} NPC owner differs from SDE"
            );
            ensure!(
                row["stationTypeID"].as_u64() == Some(raw_station.type_id as u64),
                "station {id} type differs from SDE"
            );
            ensure!(
                row["categoryID"].as_u64() == Some(3),
                "station {id} is not the Station category"
            );
            let station = data
                .station(id)
                .ok_or_else(|| anyhow!("missing generated station {id}"))?;
            let system = data
                .solar_system(raw_station.system)
                .ok_or_else(|| anyhow!("station {id} system missing"))?;
            ensure!(
                station.solar_system_id == raw_station.system
                    && station.region_id == system.region_id
                    && station.constellation_id == system.constellation_id,
                "station {id} geometry differs from authoritative static data"
            );
            ensure!(
                stations
                    .insert(
                        id,
                        NpcStation {
                            station_id: id,
                            name: station.station_name.clone(),
                            owner_id: raw_station.owner,
                            solar_system_id: station.solar_system_id,
                            constellation_id: station.constellation_id,
                            region_id: station.region_id,
                            region_name: station.region_name.clone(),
                            nearest_hub_jumps: None
                        }
                    )
                    .is_none(),
                "duplicate generated station {id}"
            );
        }
        for id in CANONICAL_HUB_STATION_IDS {
            ensure!(
                stations.contains_key(&id),
                "canonical hub {id} is not an eligible NPC station"
            );
        }
        let gate_path = raw.join("mapStargates.jsonl");
        let mut topology_available = false;
        let mut topology_warning = None;
        if gate_path.is_file() {
            let gates: BTreeMap<_, _> = jsonl::<Gate>(&gate_path)?
                .into_iter()
                .map(|g| (g.id, g))
                .collect();
            let mut adjacency = BTreeMap::<u32, BTreeSet<u32>>::new();
            for gate in gates.values() {
                let reciprocal = gates
                    .get(&gate.destination.gate)
                    .ok_or_else(|| anyhow!("missing stargate destination"))?;
                ensure!(
                    reciprocal.system == gate.destination.system
                        && reciprocal.destination.system == gate.system
                        && reciprocal.destination.gate == gate.id,
                    "nonreciprocal SDE stargate"
                );
                ensure!(
                    data.solar_system(gate.system).is_some()
                        && data.solar_system(gate.destination.system).is_some(),
                    "stargate system absent from generated static data"
                );
                adjacency
                    .entry(gate.system)
                    .or_default()
                    .insert(gate.destination.system);
            }
            let roots: Vec<_> = CANONICAL_HUB_STATION_IDS
                .iter()
                .map(|id| stations[id].solar_system_id)
                .collect();
            let distances = jump_distances(&adjacency, &roots);
            for station in stations.values_mut() {
                station.nearest_hub_jumps = distances.get(&station.solar_system_id).copied();
            }
            topology_available = true;
        } else {
            topology_warning =
                Some("Authoritative stargate topology absent; remoteness is unavailable.");
        }
        let mut files = Vec::new();
        for name in [
            "_sde.jsonl",
            "npcStations.jsonl",
            "npcCorporations.jsonl",
            "mapStargates.jsonl",
        ] {
            let path = raw.join(name);
            if path.is_file() {
                files.push(json!({"path":path,"sha256":sha256_hex(&fs::read(&path)?)}));
            }
        }
        Ok(Self {
            provenance: json!({"sde_build":build,"eligibility":"CCP npcStations + nondeleted npcCorporations + matching generated Station metadata","files":files,"topology_warning":topology_warning}),
            stations,
            topology_available,
        })
    }
    pub fn require(&self, id: u64) -> Result<&NpcStation> {
        self.stations.get(&id).ok_or_else(|| anyhow!("Location {id} is not an authoritative eligible NPC station; player structures and unknown locations are forbidden"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn topology_is_multi_source_hops_not_geometry() {
        let graph = BTreeMap::from([
            (1, BTreeSet::from([2])),
            (2, BTreeSet::from([1, 3])),
            (3, BTreeSet::from([2, 4])),
            (4, BTreeSet::from([3])),
        ]);
        assert_eq!(
            jump_distances(&graph, &[1, 4]),
            BTreeMap::from([(1, 0), (2, 1), (3, 1), (4, 0)])
        );
        assert_eq!(jump_distances(&graph, &[1])[&4], 3);
        assert!(!jump_distances(&graph, &[1]).contains_key(&5));
    }
    #[test]
    fn unknown_locations_and_player_structures_fail_closed() {
        let inventory = Inventory {
            stations: BTreeMap::new(),
            provenance: json!({}),
            topology_available: false,
        };
        assert!(inventory.require(1_000_000_000_000).is_err());
        assert!(inventory.require(60_003_760).is_err()); // A plausible ID does not prove NPC ownership.
    }
}
