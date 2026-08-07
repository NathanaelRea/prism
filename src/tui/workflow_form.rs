use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use crate::workflow::definition::DefinitionSnapshot;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FormField {
    pub section: String,
    pub name: String,
    pub value: String,
    pub requirement: String,
    pub kind: String,
    port: String,
    property: Option<String>,
}

pub(crate) struct WorkflowInputForm {
    fields: Vec<FormField>,
    selected: usize,
    schemas: BTreeMap<String, Value>,
    ports: BTreeMap<String, crate::PortDefinition>,
    initial: BTreeMap<String, Value>,
}

impl WorkflowInputForm {
    pub(crate) fn new(
        snapshot: &DefinitionSnapshot,
        initial: BTreeMap<String, Value>,
    ) -> Result<Self, String> {
        let mut fields = Vec::new();
        for (name, port) in &snapshot.definition.inputs {
            if initial.contains_key(name) {
                continue;
            }
            let schema = snapshot.schemas.get(&port.schema).ok_or_else(|| {
                format!(
                    "workflow input '{name}' uses unavailable schema {}",
                    port.schema
                )
            })?;
            append_fields(&mut fields, name, port.required, &port.schema, schema);
        }
        Ok(Self {
            fields,
            selected: 0,
            schemas: snapshot.schemas.clone(),
            ports: snapshot.definition.inputs.clone(),
            initial,
        })
    }

    pub(crate) fn fields(&self) -> &[FormField] {
        &self.fields
    }

    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    pub(crate) fn next(&mut self) {
        if !self.fields.is_empty() {
            self.selected = (self.selected + 1) % self.fields.len();
        }
    }

    pub(crate) fn previous(&mut self) {
        if !self.fields.is_empty() {
            self.selected = self
                .selected
                .checked_sub(1)
                .unwrap_or(self.fields.len() - 1);
        }
    }

    pub(crate) fn push(&mut self, ch: char) {
        if let Some(field) = self.fields.get_mut(self.selected) {
            field.value.push(ch);
        }
    }

    pub(crate) fn pop(&mut self) {
        if let Some(field) = self.fields.get_mut(self.selected) {
            field.value.pop();
        }
    }

    pub(crate) fn submit(&self) -> Result<BTreeMap<String, Value>, String> {
        let mut values = self.initial.clone();
        let mut object_ports: BTreeMap<&str, Map<String, Value>> = BTreeMap::new();
        for field in &self.fields {
            if field.property.is_some() && self.ports[&field.port].required {
                object_ports.entry(&field.port).or_default();
            }
            let text = field.value.trim();
            if text.is_empty() {
                continue;
            }
            let value = parse_field(text, &field.kind)
                .map_err(|error| format!("{}.{}: {error}", field.section, field.name))?;
            if let Some(property) = &field.property {
                object_ports
                    .entry(&field.port)
                    .or_default()
                    .insert(property.clone(), value);
            } else {
                values.insert(field.port.clone(), value);
            }
        }
        for (port, object) in object_ports {
            values.insert(port.to_string(), Value::Object(object));
        }
        for (name, port) in &self.ports {
            let Some(value) = values.get(name) else {
                if port.required {
                    return Err(format!("{name} is required"));
                }
                continue;
            };
            if let Some(schema) = self.schemas.get(&port.schema) {
                crate::workflow::schema::validate_value(value, schema)
                    .map_err(|error| format!("{name}: {error}"))?;
            }
        }
        Ok(values)
    }
}

fn append_fields(
    fields: &mut Vec<FormField>,
    port_name: &str,
    port_required: bool,
    schema_id: &str,
    schema: &Value,
) {
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        fields.push(FormField {
            section: format!("{port_name} · {schema_id}"),
            name: "value".into(),
            value: String::new(),
            requirement: requirement(port_required),
            kind: schema_kind(schema),
            port: port_name.into(),
            property: None,
        });
        return;
    }

    let direct_required = names(schema.get("required"));
    let mut property_names = schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| properties.keys().cloned().collect::<BTreeSet<_>>())
        .unwrap_or_default();
    property_names.extend(direct_required.iter().cloned());
    let mut alternative_required = BTreeSet::new();
    if let Some(alternatives) = schema.get("anyOf").and_then(Value::as_array) {
        for alternative in alternatives {
            alternative_required.extend(names(alternative.get("required")));
        }
        property_names.extend(alternative_required.iter().cloned());
    }
    if property_names.is_empty() {
        fields.push(FormField {
            section: format!("{port_name} · {schema_id}"),
            name: "JSON value".into(),
            value: String::new(),
            requirement: requirement(port_required),
            kind: "object".into(),
            port: port_name.into(),
            property: None,
        });
        return;
    }
    for property in property_names {
        let child = schema
            .get("properties")
            .and_then(|value| value.get(&property));
        fields.push(FormField {
            section: format!("{port_name} · {schema_id}"),
            name: property.clone(),
            value: String::new(),
            requirement: if direct_required.contains(&property) {
                "required".into()
            } else if port_required && alternative_required.contains(&property) {
                "one required".into()
            } else {
                "optional".into()
            },
            kind: child.map(schema_kind).unwrap_or_else(|| "json".into()),
            port: port_name.into(),
            property: Some(property),
        });
    }
}

fn names(value: Option<&Value>) -> BTreeSet<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn requirement(required: bool) -> String {
    if required { "required" } else { "optional" }.into()
}

fn schema_kind(schema: &Value) -> String {
    schema
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("json")
        .to_string()
}

fn parse_field(text: &str, kind: &str) -> Result<Value, String> {
    if kind == "string" {
        return Ok(Value::String(text.into()));
    }
    serde_json::from_str(text).map_err(|error| match kind {
        "object" => format!("enter a JSON object ({error})"),
        "array" => format!("enter a JSON array ({error})"),
        "boolean" => "enter true or false".into(),
        "integer" | "number" => "enter a number".into(),
        _ => format!("enter valid JSON ({error})"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::definition::DefinitionSnapshot;

    fn snapshot() -> DefinitionSnapshot {
        serde_json::from_value(serde_json::json!({
            "snapshot_schema_version":1,
            "definition":{"id":"acme/run","name":"run","description":"","launch":["manual"],"tags":[],"declared_capabilities":[],"inputs":{"task":{"type":"acme/task","required":true,"from_context":false}},"outputs":{},"parameters":{},"budgets":{},"steps":[]},
            "sources":{},"implementations":{},
            "schemas":{"acme/task":{"type":"object","anyOf":[{"required":["title"]},{"required":["body"]}],"properties":{"title":{"type":"string"},"body":{"type":"string"}}}},
            "children":{},"package_revisions":{},"capabilities":[],"trusted":true,"digest":"digest"
        })).unwrap()
    }

    #[test]
    fn builds_fields_and_keeps_validation_errors_in_the_form_seam() {
        let mut form = WorkflowInputForm::new(&snapshot(), BTreeMap::new()).unwrap();
        assert_eq!(form.fields.len(), 2);
        assert!(form.submit().unwrap_err().contains("allowed input shape"));
        form.push('d');
        form.push('o');
        form.push('n');
        form.push('e');
        let values = form.submit().unwrap();
        assert_eq!(values["task"]["body"], "done");
    }
}
