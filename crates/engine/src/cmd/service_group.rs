//! Atomic import and refresh of a service's feature collections. Both native callers and the
//! asynchronous GUI use the same discovery, response validation and layer update code.

use rustgis_data::{Dataset, ogc::Offer};
use rustgis_map::{ServiceChild, ServiceGroup};
use serde_json::{Value, json};

use super::{C, CmdResult, MAX_GROUP_BYTES, Session, capabilities, ps, service_json};

pub fn commands() -> Vec<C> {
    vec![
        C::new("service.import", "Import Service", "Home › Add", import).params(
            r#"{"url": "an OGC API landing page or collections address", "type"?, "name"?: "group name (default My OGC Layer)", "text"?, "documents"?: "as service.layers", "responses"?: [{"url": "features URL", "text": "downloaded GeoJSON"}]} ("group" and "layer" are rejected: use service.refresh)"#,
        ),
        C::new("service.refresh", "Refresh", "Layers", refresh).params(
            r#"{"group"?: id, "layer"?: "one child layer id or name", "text"?, "documents"?, "responses"?: "as service.import"}"#,
        ),
        C::new("service.groupPlan", "Service Import Requests", "Layers", plan).view().params(
            r#"{"url"?, "type"?, "name"?, "group"?: id, "layer"?: "one child layer", "sourceOnly"?: "return the connection without fetching", "text"?, "documents"?} → request, layers, and next while discovery is incomplete"#,
        ),
        C::new("service.group", "Service Group", "Layers", set_group).params(
            r#"{"group"?: id, "layer"?: "a child identifying its group", "name"?: text, "expanded"?: bool, "visible"?: "set all children", "remove"?: "remove the group and its children"}"#,
        ),
    ]
}

fn target(s: &Session, p: &Value) -> Result<Option<(ServiceGroup, Option<u32>)>, String> {
    let layer = p.get("layer").map(|_| s.layer_id(p)).transpose()?;
    if p.get("group").is_none() && layer.is_none() {
        return Ok(None);
    }
    let id = p.get("group").map(|v| v.as_u64().and_then(|n| u32::try_from(n).ok()).ok_or("invalid group id")).transpose()?;
    let group = s
        .map()
        .service_groups
        .iter()
        .find(|g| id.is_none_or(|id| g.id == id) && layer.is_none_or(|id| g.children.iter().any(|c| c.layer == id)))
        .ok_or("no such service group or child")?;
    Ok(Some((group.clone(), layer)))
}

fn request(s: &Session, p: &Value) -> CmdResult {
    let mut q = p.clone();
    if let Some((group, _)) = target(s, p)? {
        q["url"] = json!(group.url);
        q["type"] = json!(group.kind);
        q["name"] = json!(group.name);
    }
    super::url_of(&q)?;
    Ok(q)
}

fn plan(s: &mut Session, p: &Value) -> CmdResult {
    if p.get("sourceOnly").and_then(Value::as_bool) == Some(true) {
        return request(s, p);
    }
    listing(s, p)
}

fn listing(s: &mut Session, p: &Value) -> CmdResult {
    let q = request(s, p)?;
    let mut found = capabilities(&q)?;
    if let Some(error) = found.error {
        return Err(error);
    }
    if found.excluded_layers {
        return Err("the service contains disallowed addresses; the group was not changed".into());
    }
    if !found.service.kind.starts_with("OGC API") {
        return Err("group import requires an OGC API feature service".into());
    }
    let child = target(s, p)?.and_then(|(g, id)| id.and_then(|id| g.children.into_iter().find(|c| c.layer == id)));
    let mut sources = Vec::new();
    let mut identities = std::collections::HashSet::new();
    let mut selected = Vec::new();
    for (layer, source) in found.service.layers.into_iter().zip(found.sources) {
        if !matches!(layer.offer, Offer::Features { .. }) {
            continue;
        }
        if child.as_ref().is_some_and(|c| c.collection != layer.name || c.source_url != source) {
            continue;
        }
        if !identities.insert((source.clone(), layer.name.clone())) {
            return Err("the service advertises duplicate collection identifiers".into());
        }
        selected.push(layer);
        sources.push(source);
    }
    if selected.len() > 2000 {
        return Err("a service group supports at most 2000 collections".into());
    }
    if found.next.is_none() && child.is_some() && selected.is_empty() {
        return Err("the collection is no longer advertised by this service".into());
    }
    found.service.layers = selected;
    let mut out = service_json(&found.service);
    if let Some(layers) = out.get_mut("layers").and_then(Value::as_array_mut) {
        for (layer, source) in layers.iter_mut().zip(sources) {
            layer["source_url"] = json!(source);
        }
    }
    if let Some(next) = found.next {
        out["next"] = json!(next);
    }
    out["request"] = json!({"url": q.get("url"), "type": ps(&q, "type").unwrap_or("ogcapi")});
    Ok(out)
}

#[cfg(not(target_arch = "wasm32"))]
fn download(url: &str, max: usize) -> Result<Vec<u8>, String> {
    crate::render::rustgis_tiles::cache::download_blocking_limited(url, max)
}

#[cfg(target_arch = "wasm32")]
fn download(_: &str, _: usize) -> Result<Vec<u8>, String> {
    Err("the browser must supply downloaded feature responses".into())
}

fn read(s: &mut Session, p: &Value) -> Result<(Value, Vec<(ServiceChild, Dataset)>), String> {
    let listing = listing(s, p)?;
    if listing.get("next").is_some() {
        return Err("complete service discovery before importing or refreshing".into());
    }
    let layers = listing.get("layers").and_then(Value::as_array).ok_or("missing collections")?;
    let responses = p.get("responses").map(|v| v.as_array().ok_or("responses must be an array")).transpose()?;
    if responses.is_some_and(|r| r.len() > 2000) {
        return Err("at most 2000 feature responses may be supplied".into());
    }
    let mut bytes_read = 0usize;
    let mut reads = Vec::new();
    for layer in layers {
        let name = ps(layer, "title").unwrap_or("Collection");
        let url = layer.get("offer").and_then(|o| ps(o, "url")).ok_or("missing features address")?;
        let bytes = match responses {
            Some(responses) => {
                let text = responses
                    .iter()
                    .find(|r| ps(r, "url") == Some(url))
                    .and_then(|r| ps(r, "text"))
                    .ok_or_else(|| format!("{name}: missing downloaded response"))?;
                if text.len() > MAX_GROUP_BYTES.saturating_sub(bytes_read) {
                    return Err("service group data exceeds 64 MB".into());
                }
                text.as_bytes().to_vec()
            }
            None => download(url, MAX_GROUP_BYTES.saturating_sub(bytes_read))?,
        };
        bytes_read = bytes_read.saturating_add(bytes.len());
        if bytes_read > MAX_GROUP_BYTES {
            return Err("service group data exceeds 64 MB".into());
        }
        let bytes = super::features_answer(s, name, bytes);
        let text = std::str::from_utf8(&bytes).map_err(|e| format!("{name}: {e}"))?;
        let (mut dataset, report) = rustgis_data::io::read_geojson(text, name).map_err(|e| format!("{name}: {e}"))?;
        // Empty collections remain visible as children and can acquire geometry on refresh.
        dataset.source = Some(url.to_string());
        for message in report.messages {
            s.notify(message, false);
        }
        reads.push((
            ServiceChild {
                layer: 0,
                collection: ps(layer, "name").unwrap_or_default().to_string(),
                source_url: ps(layer, "source_url").unwrap_or_default().to_string(),
            },
            dataset,
        ));
    }
    Ok((listing, reads))
}

fn import(s: &mut Session, p: &Value) -> CmdResult {
    if p.get("group").is_some() || p.get("layer").is_some() {
        return Err("use service.refresh for an existing group or child".into());
    }
    let (listing, reads) = read(s, p)?;
    if reads.is_empty() {
        return Err("the service has no feature collections".into());
    }
    let mut children = Vec::new();
    let mut extent = rustgis_geom::Extent::EMPTY;
    for (mut child, dataset) in reads {
        child.layer = crate::cmd::layer::add_dataset(s, dataset);
        if let Some(layer) = s.map().layer(child.layer) {
            extent = extent.union(&layer.extent());
        }
        children.push(child);
    }
    let name = ps(p, "name").map(str::trim).filter(|n| !n.is_empty()).unwrap_or("My OGC Layer").chars().take(200).collect();
    let req = listing.get("request").ok_or("missing service request")?;
    let group = ServiceGroup {
        id: 0,
        name,
        url: ps(req, "url").unwrap_or_default().into(),
        kind: ps(req, "type").unwrap_or("ogcapi").into(),
        expanded: true,
        children,
    };
    let ids: Vec<u32> = group.children.iter().map(|c| c.layer).collect();
    let id = s.map_mut().add_service_group(group);
    if !extent.is_empty() {
        s.zoom_to(&extent);
    }
    Ok(json!({"group":id,"layers":ids}))
}

fn refresh(s: &mut Session, p: &Value) -> CmdResult {
    let (old, child_id) = target(s, p)?.ok_or("group or layer is required")?;
    let hidden = old.children.iter().any(|c| s.map().layer(c.layer).is_some())
        && old.children.iter().filter_map(|c| s.map().layer(c.layer)).all(|l| !l.visible);
    let (_, reads) = read(s, p)?;
    let mut children = Vec::new();
    for (mut child, mut dataset) in reads {
        let existing = old
            .children
            .iter()
            .find(|c| c.collection == child.collection && c.source_url == child.source_url)
            .and_then(|c| s.map_mut().layer_mut(c.layer));
        if let Some(layer) = existing {
            if dataset.geometry_type == rustgis_data::GeometryType::None {
                dataset.geometry_type = layer.geometry_type();
            }
            let changed_type = layer.geometry_type() != dataset.geometry_type;
            dataset.name = layer.name.clone();
            layer.dataset = dataset;
            layer.selection.clear();
            let invalid_renderer = match &layer.renderer {
                rustgis_map::Renderer::Single(_) => false,
                rustgis_map::Renderer::Unique { field, .. } => layer.dataset.field_index(field).is_none(),
                rustgis_map::Renderer::Graduated { field, .. } | rustgis_map::Renderer::Proportional { field, .. } => {
                    layer.dataset.field_index(field).and_then(|i| layer.dataset.fields.get(i)).is_none_or(|f| !f.ty.is_numeric())
                }
            };
            if changed_type || invalid_renderer {
                layer.renderer = rustgis_map::Renderer::Single(rustgis_map::Symbol::default_for(layer.geometry_type(), layer.id));
            }
            if layer.dataset.field_index(&layer.labels.field).is_none() {
                layer.labels.enabled = false;
                layer.labels.field.clear();
            }
            if layer.definition_query.as_deref().is_some_and(|q| rustgis_data::query::parse(q, &layer.dataset).is_err()) {
                layer.definition_query = None;
            }
            layer.display_filters.retain(|f| rustgis_data::query::parse(&f.where_, &layer.dataset).is_ok());
            layer.touch_geometry();
            child.layer = layer.id;
        } else {
            child.layer = crate::cmd::layer::add_dataset(s, dataset);
            if hidden && let Some(layer) = s.map_mut().layer_mut(child.layer) {
                layer.visible = false;
            }
        }
        children.push(child);
    }
    let ids: Vec<u32> = children.iter().map(|c| c.layer).collect();
    if child_id.is_none() {
        s.map_mut().layers.retain(|l| !old.children.iter().any(|c| c.layer == l.id) || ids.contains(&l.id));
        let group = s.map_mut().service_groups.iter_mut().find(|g| g.id == old.id).ok_or("the group was removed")?;
        group.children = children;
    }
    Ok(json!({"group":old.id,"layers":ids}))
}

fn set_group(s: &mut Session, p: &Value) -> CmdResult {
    let (group, _) = target(s, p)?.ok_or("group is required")?;
    if p.get("remove").and_then(Value::as_bool) == Some(true) {
        s.map_mut().layers.retain(|l| !group.children.iter().any(|c| c.layer == l.id));
        s.map_mut().service_groups.retain(|g| g.id != group.id);
        return Ok(Value::Null);
    }
    if let Some(visible) = p.get("visible").and_then(Value::as_bool) {
        for layer in &mut s.map_mut().layers {
            if group.children.iter().any(|c| c.layer == layer.id) {
                layer.visible = visible;
            }
        }
    }
    let g = s.map_mut().service_groups.iter_mut().find(|g| g.id == group.id).ok_or("no such service group")?;
    if let Some(name) = ps(p, "name") {
        let name: String = name.trim().chars().take(200).collect();
        if name.is_empty() {
            return Err("name is required".into());
        }
        g.name = name;
    }
    if let Some(expanded) = p.get("expanded").and_then(Value::as_bool) {
        g.expanded = expanded;
    }
    Ok(json!({"group":group.id}))
}
