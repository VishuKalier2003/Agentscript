// Repository zones: persistent, semantically selected classifications (criticality, autonomy,
// safety state) re-resolved against the inventory on every run. Zones are an input to
// authorization that can only restrict; they never grant anything and never relax a contract.

pub(crate) mod map;
pub(crate) mod model;
pub(crate) mod recommend;
pub(crate) mod resolve;
pub(crate) mod review;
pub(crate) mod view;

#[cfg(test)]
mod tests;

use serde_json::{json, Value};

use crate::inventory::graph::Graph;
use crate::inventory::{discover, Inventory, Options};
use crate::repository::root;
use model::set_version;
use resolve::{Effective, Resolution};

/** Version of the zones JSON layout */
const ZONES_FORMAT: u64 = 1;

/** The zones of the repository resolved against its current inventory
 * Fields
    - inventory: Inventory - current inventory (incrementally refreshed)
    - resolution: Resolution - resolved zones, combined constraints, and conflicts
    - problems: Vec<String> - malformed zone files and duplicate zone ids
    - version: String - version of the zone set
*/
pub(crate) struct Zones {
    pub(crate) inventory: Inventory,
    pub(crate) resolution: Resolution,
    pub(crate) problems: Vec<String>,
    pub(crate) version: String,
}

/** Load the zone definitions from .crane/zones and resolve them against a freshly (and
 * incrementally) discovered inventory, recording each selector's last good resolution in
 * .crane/runtime/zones/resolution.json so later renames and deletions can be explained
 * Input
    - None
 * Output
    - Result<Zones, String>
    - Error if Crane is not initialized, discovery fails, or the state cannot be written
*/
pub(crate) fn inspect() -> Result<Zones, String> {
    let crane = root()?;
    let (zones, problems) = model::load(&crane)?;
    let version = set_version(&zones);
    let inventory = discover(&Options { full: false })?;
    let state = crane.join("runtime").join("zones").join("resolution.json");
    let resolution = resolve::resolve(&inventory, zones, &state)?;
    Ok(Zones {
        inventory,
        resolution,
        problems,
        version,
    })
}

impl Zones {
    /** Serialize combined constraints
     * Input
        - effective: &Effective - constraints
     * Output
        - Value JSON object
    */
    fn effective_json(&self, effective: &Effective) -> Value {
        json!({
            "zones": effective.zones.iter().map(|index| self.resolution.zones[*index].zone.zone_id.clone()).collect::<Vec<_>>(),
            "criticality": effective.criticality.name(),
            "autonomy": effective.autonomy.name(),
            "safety_state": effective.state.name(),
        })
    }

    /** Serialize the zones, the machine-readable form of crane zones, with the zone map's state;
     * with a zone id, only that zone (and the constraints on its entities and files) is included
     * Input
        - only: Option<&str> - zone id to restrict the output to
     * Output
        - Value JSON object
    */
    pub(crate) fn to_json(&self, only: Option<&str>) -> Value {
        let graph = &self.inventory.graph;
        let snapshot = &self.inventory.snapshot;
        let selected = |index: usize| {
            only.is_none_or(|zone| self.resolution.zones[index].zone.zone_id == zone)
        };
        let zones = self
            .resolution
            .zones
            .iter()
            .enumerate()
            .filter(|(index, _)| selected(*index))
            .map(|(_, result)| {
                let zone = &result.zone;
                json!({
                    "zone_id": zone.zone_id,
                    "source": zone.source,
                    "version": zone.version,
                    "criticality": zone.criticality.name(),
                    "default_autonomy": zone.default_autonomy.name(),
                    "effective_autonomy": result.autonomy.name(),
                    "safety_state": zone.safety_state.name(),
                    "effective_safety_state": result.state.name(),
                    "policy_reference": zone.policy_reference,
                    "selectors": result.selectors.iter().map(|selector| json!({
                        "selector": selector.selector.text(),
                        "semantic": selector.selector.kind.semantic(),
                        "status": selector.status,
                        "targets": selector.targets,
                        "entities": selector.entities.len(),
                        "files": selector.files.len(),
                        "previous_targets": selector.previous,
                        "rename_candidates": selector.candidates,
                    })).collect::<Vec<_>>(),
                    "entities": result.entities.iter().map(|index| graph.entities[*index].id.clone()).collect::<Vec<_>>(),
                    "files": result.files.iter().map(|index| snapshot.files[*index].path.clone()).collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>();
        let unresolved = self
            .resolution
            .zones
            .iter()
            .enumerate()
            .filter(|(index, _)| selected(*index))
            .flat_map(|(_, result)| {
                result
                    .selectors
                    .iter()
                    .filter(|selector| selector.status != "resolved")
                    .map(move |selector| {
                        json!({
                            "zone_id": result.zone.zone_id,
                            "selector": selector.selector.text(),
                            "status": selector.status,
                            "targets": selector.targets,
                            "previous_targets": selector.previous,
                            "rename_candidates": selector.candidates,
                        })
                    })
            })
            .collect::<Vec<_>>();
        let involved = |zones: &[usize]| zones.iter().any(|index| selected(*index));
        let entities = self
            .resolution
            .entities
            .iter()
            .filter(|(_, effective)| involved(&effective.zones))
            .map(|(index, effective)| {
                let entity = &graph.entities[*index];
                let mut value = self.effective_json(effective);
                value["id"] = json!(entity.id);
                value["kind"] = json!(Graph::symbol(snapshot, entity).kind.name());
                value["file"] = json!(snapshot.files[entity.file].path);
                value["contracts"] = json!(entity.contracts);
                value
            })
            .collect::<Vec<_>>();
        let files = self
            .resolution
            .files
            .iter()
            .filter(|(_, effective)| involved(&effective.zones))
            .map(|(index, effective)| {
                let mut value = self.effective_json(effective);
                value["path"] = json!(snapshot.files[*index].path);
                value
            })
            .collect::<Vec<_>>();
        let conflicts = self
            .resolution
            .conflicts
            .iter()
            .filter(|conflict| {
                only.is_none_or(|zone| conflict.zones.iter().any(|name| name == zone))
            })
            .map(|conflict| {
                json!({
                    "kind": conflict.kind,
                    "zones": conflict.zones,
                    "message": conflict.message,
                    "entities": conflict.entities,
                })
            })
            .collect::<Vec<_>>();
        json!({
            "zones_format": ZONES_FORMAT,
            // The compact zone map's state: approved or changed, shadowed and unresolved lines
            "map": map::status(&self.inventory).unwrap_or_else(|error| json!({"state": "unreadable", "errors": [error]})),
            "grants_permissions": false,
            "zone_set_version": self.version,
            "contract_version": self.inventory.contract_version,
            "zones": zones,
            "unresolved": unresolved,
            "conflicts": conflicts,
            "entities": entities,
            "files": files,
            "problems": self.problems,
        })
    }
}
