use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use prism_extension_protocol::{
    ArtifactSchemaDescriptor, ExtensionDescriptor, ImplementationDescriptor, InputDescriptor,
    NotificationChannelDescriptor, RendererDescriptor, StepClass, TriggerDescriptor,
};

#[derive(Clone, Debug, Default)]
pub struct DescriptorRegistry {
    implementations: BTreeMap<String, ImplementationDescriptor>,
    schemas: BTreeMap<String, ArtifactSchemaDescriptor>,
    inputs: BTreeMap<String, InputDescriptor>,
    renderers: BTreeMap<String, RendererDescriptor>,
    triggers: BTreeMap<String, TriggerDescriptor>,
    channels: BTreeMap<String, NotificationChannelDescriptor>,
}

impl DescriptorRegistry {
    pub fn register(&mut self, descriptor: &ExtensionDescriptor) -> Result<(), RegistryError> {
        let mut candidate = self.clone();
        for schema in &descriptor.artifact_schemas {
            validate_qualified(&schema.id)?;
            if !schema.schema.is_object() {
                return Err(RegistryError::InvalidSchema(schema.id.clone()));
            }
            validate_schema_shape(schema)?;
            if let Some(existing) = candidate.schemas.get(&schema.id) {
                if existing.schema != schema.schema {
                    return Err(RegistryError::IncompatibleSchema(schema.id.clone()));
                }
            } else {
                candidate.schemas.insert(schema.id.clone(), schema.clone());
            }
        }
        for implementation in &descriptor.implementations {
            validate_qualified(&implementation.id)?;
            for ports in [&implementation.inputs, &implementation.outputs] {
                let mut names = BTreeSet::new();
                for port in ports {
                    if port.name.trim().is_empty() || port.schema.trim().is_empty() {
                        return Err(RegistryError::InvalidDescriptor(implementation.id.clone()));
                    }
                    validate_qualified(&port.schema)?;
                    if !candidate.schemas.contains_key(&port.schema) {
                        return Err(RegistryError::UnknownSchema(port.schema.clone()));
                    }
                    if !names.insert(&port.name) {
                        return Err(RegistryError::DuplicateId(format!(
                            "{}:{}",
                            implementation.id, port.name
                        )));
                    }
                }
            }
            let declares_protected_mutation = implementation
                .capabilities
                .iter()
                .any(|capability| capability.ends_with(":write") || capability.ends_with("_write"));
            if declares_protected_mutation
                && implementation.effect_boundary == prism_extension_protocol::EffectBoundary::None
            {
                return Err(RegistryError::MissingEffectDisclosure(
                    implementation.id.clone(),
                ));
            }
            if implementation.class == StepClass::Gate
                && implementation.effect_boundary
                    == prism_extension_protocol::EffectBoundary::Brokered
            {
                return Err(RegistryError::InvalidEffectDisclosure(
                    implementation.id.clone(),
                ));
            }
            if candidate
                .implementations
                .insert(implementation.id.clone(), implementation.clone())
                .is_some()
            {
                return Err(RegistryError::DuplicateId(implementation.id.clone()));
            }
        }
        for input in &descriptor.input_support {
            validate_qualified(&input.schema_id)?;
            if !candidate.schemas.contains_key(&input.schema_id) {
                return Err(RegistryError::UnknownSchema(input.schema_id.clone()));
            }
            insert_map(
                &mut candidate.inputs,
                &input.schema_id,
                input.clone(),
                "input support",
            )?;
        }
        for renderer in &descriptor.renderers {
            validate_qualified(&renderer.schema_id)?;
            if !candidate.schemas.contains_key(&renderer.schema_id) {
                return Err(RegistryError::UnknownSchema(renderer.schema_id.clone()));
            }
            insert_map(
                &mut candidate.renderers,
                &renderer.schema_id,
                renderer.clone(),
                "renderer",
            )?;
        }
        for trigger in &descriptor.triggers {
            validate_qualified(&trigger.id)?;
            insert_map(
                &mut candidate.triggers,
                &trigger.id,
                trigger.clone(),
                "Trigger",
            )?;
        }
        for channel in &descriptor.notification_channels {
            validate_qualified(&channel.id)?;
            insert_map(
                &mut candidate.channels,
                &channel.id,
                channel.clone(),
                "notification channel",
            )?;
        }
        *self = candidate;
        Ok(())
    }

    pub fn implementation(&self, id: &str) -> Option<&ImplementationDescriptor> {
        self.implementations.get(id)
    }

    pub fn implementations(&self) -> impl Iterator<Item = &ImplementationDescriptor> {
        self.implementations.values()
    }

    pub fn implementation_class(&self, id: &str) -> Option<StepClass> {
        self.implementation(id).map(|value| value.class)
    }

    pub fn artifact_schema(&self, id: &str) -> Option<&ArtifactSchemaDescriptor> {
        self.schemas.get(id)
    }

    pub fn input_support(&self) -> impl Iterator<Item = &InputDescriptor> {
        self.inputs.values()
    }

    pub fn renderers(&self) -> impl Iterator<Item = &RendererDescriptor> {
        self.renderers.values()
    }

    pub fn triggers(&self) -> impl Iterator<Item = &TriggerDescriptor> {
        self.triggers.values()
    }

    pub fn notification_channels(&self) -> impl Iterator<Item = &NotificationChannelDescriptor> {
        self.channels.values()
    }
}

fn insert_map<T>(
    map: &mut BTreeMap<String, T>,
    id: &str,
    value: T,
    kind: &str,
) -> Result<(), RegistryError> {
    if id.trim().is_empty() || map.insert(id.to_owned(), value).is_some() {
        return Err(RegistryError::DuplicateId(format!("{kind}:{id}")));
    }
    Ok(())
}

fn validate_qualified(id: &str) -> Result<(), RegistryError> {
    crate::resource::QualifiedIdentity::new(id.to_owned())
        .map(|_| ())
        .map_err(|_| RegistryError::InvalidId(id.into()))
}

fn validate_schema_shape(schema: &ArtifactSchemaDescriptor) -> Result<(), RegistryError> {
    let object = schema
        .schema
        .as_object()
        .ok_or_else(|| RegistryError::InvalidSchema(schema.id.clone()))?;
    if object.get("type").is_some_and(|value| {
        !value.is_string()
            && !value
                .as_array()
                .is_some_and(|values| values.iter().all(serde_json::Value::is_string))
    }) || object
        .get("properties")
        .is_some_and(|value| !value.is_object())
        || object.get("required").is_some_and(|value| {
            !value
                .as_array()
                .is_some_and(|values| values.iter().all(serde_json::Value::is_string))
        })
    {
        return Err(RegistryError::InvalidSchema(schema.id.clone()));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegistryError {
    DuplicateId(String),
    InvalidId(String),
    InvalidSchema(String),
    IncompatibleSchema(String),
    UnknownSchema(String),
    InvalidDescriptor(String),
    MissingEffectDisclosure(String),
    InvalidEffectDisclosure(String),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateId(id) => write!(formatter, "duplicate extension descriptor id '{id}'"),
            Self::InvalidId(id) => write!(
                formatter,
                "invalid qualified extension descriptor id '{id}'"
            ),
            Self::InvalidSchema(id) => {
                write!(formatter, "Artifact schema '{id}' must be a JSON object")
            }
            Self::IncompatibleSchema(id) => write!(
                formatter,
                "Artifact schema '{id}' is incompatible with its registered definition"
            ),
            Self::UnknownSchema(id) => write!(
                formatter,
                "descriptor references unknown Artifact schema '{id}'"
            ),
            Self::InvalidDescriptor(id) => {
                write!(formatter, "extension descriptor '{id}' is incomplete")
            }
            Self::MissingEffectDisclosure(id) => write!(
                formatter,
                "mutating implementation '{id}' must disclose brokered or unbrokered effects"
            ),
            Self::InvalidEffectDisclosure(id) => write!(
                formatter,
                "Gate implementation '{id}' cannot claim brokered mutation guarantees"
            ),
        }
    }
}

impl std::error::Error for RegistryError {}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_extension_protocol::{ArtifactSchemaDescriptor, ExtensionDescriptor};
    use serde_json::json;

    #[test]
    fn dangling_port_schema_is_rejected() {
        use prism_extension_protocol::{ImplementationDescriptor, PortDescriptor, StepClass};
        let descriptor = ExtensionDescriptor {
            implementations: vec![ImplementationDescriptor {
                id: "acme.test/action".into(),
                class: StepClass::Action,
                inputs: vec![PortDescriptor {
                    name: "value".into(),
                    schema: "acme.test/missing".into(),
                    required: true,
                }],
                outputs: Vec::new(),
                capabilities: Vec::new(),
                targets: Vec::new(),
                effect_boundary: Default::default(),
            }],
            ..Default::default()
        };
        assert!(matches!(
            DescriptorRegistry::default().register(&descriptor),
            Err(RegistryError::UnknownSchema(_))
        ));
    }

    #[test]
    fn duplicate_implementation_ids_are_rejected() {
        use prism_extension_protocol::{ImplementationDescriptor, StepClass};
        let implementation = ImplementationDescriptor {
            id: "acme.test/action".into(),
            class: StepClass::Action,
            inputs: Vec::new(),
            outputs: Vec::new(),
            capabilities: Vec::new(),
            targets: Vec::new(),
            effect_boundary: Default::default(),
        };
        let descriptor = ExtensionDescriptor {
            implementations: vec![implementation.clone(), implementation],
            ..Default::default()
        };
        assert!(matches!(
            DescriptorRegistry::default().register(&descriptor),
            Err(RegistryError::DuplicateId(_))
        ));
    }

    #[test]
    fn mutating_implementations_must_disclose_the_effect_boundary() {
        use prism_extension_protocol::{ImplementationDescriptor, StepClass};
        let descriptor = ExtensionDescriptor {
            implementations: vec![ImplementationDescriptor {
                id: "acme.test/mutator".into(),
                class: StepClass::Action,
                inputs: vec![],
                outputs: vec![],
                capabilities: vec!["provider:write".into()],
                targets: vec!["local".into()],
                effect_boundary: Default::default(),
            }],
            ..Default::default()
        };
        assert!(matches!(
            DescriptorRegistry::default().register(&descriptor),
            Err(RegistryError::MissingEffectDisclosure(_))
        ));
    }

    #[test]
    fn registration_reuses_identical_schemas_and_rejects_incompatible_ones() {
        let descriptor = ExtensionDescriptor {
            artifact_schemas: vec![ArtifactSchemaDescriptor {
                id: "acme.test/value".into(),
                schema: json!({"type":"object"}),
            }],
            ..Default::default()
        };
        let mut registry = DescriptorRegistry::default();
        registry.register(&descriptor).unwrap();
        registry.register(&descriptor).unwrap();
        let incompatible = ExtensionDescriptor {
            artifact_schemas: vec![ArtifactSchemaDescriptor {
                id: "acme.test/value".into(),
                schema: json!({"type":"string"}),
            }],
            ..Default::default()
        };
        assert!(matches!(
            registry.register(&incompatible),
            Err(RegistryError::IncompatibleSchema(_))
        ));
    }
}
