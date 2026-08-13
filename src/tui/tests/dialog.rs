use crate::view::{ChoiceList, FormFieldKind, KeyChoice, OrderedToggleItem};

use super::super::{
    append_line_paste, confirmation_result, create_session_fields, move_enabled_ordered_item,
    selectable_choice_key, toggle_item_in_place, toggle_ordered_item,
    update_create_session_variant_field,
};

#[test]
fn create_session_fields_include_multiline_prompt_and_harness_defaults() {
    let fields = create_session_fields(&crate::harness::AgentSelectionOptions {
        models: vec![crate::harness::AgentModelOption {
            id: "provider/model".into(),
            variants: vec!["low".into(), "high".into()],
        }],
        variants: Vec::new(),
    });

    assert_eq!(fields.len(), 3);
    assert!(matches!(
        fields[0].kind,
        FormFieldKind::TextArea { height: 6 }
    ));
    let FormFieldKind::Enum { options } = &fields[1].kind else {
        panic!("model field must be an enum");
    };
    assert_eq!(options, &["Harness default", "provider/model"]);
    let FormFieldKind::Enum { options } = &fields[2].kind else {
        panic!("variant field must be an enum");
    };
    assert_eq!(options, &["Harness default"]);

    let mut fields = fields;
    fields[1].value = "provider/model".into();
    update_create_session_variant_field(
        &mut fields,
        &crate::harness::AgentSelectionOptions {
            models: vec![crate::harness::AgentModelOption {
                id: "provider/model".into(),
                variants: vec!["low".into(), "high".into()],
            }],
            variants: Vec::new(),
        },
    );
    let FormFieldKind::Enum { options } = &fields[2].kind else {
        panic!("variant field must be an enum");
    };
    assert_eq!(options, &["Harness default", "low", "high"]);
}

#[test]
fn confirmation_empty_answer_uses_the_passed_default() {
    assert_eq!(confirmation_result("", true), Some(true));
    assert_eq!(confirmation_result("", false), Some(false));
}

#[test]
fn confirmation_yes_and_no_override_the_default() {
    assert_eq!(confirmation_result("y", false), Some(true));
    assert_eq!(confirmation_result("n", true), Some(false));
}

#[test]
fn confirmation_rejects_unknown_answers() {
    assert_eq!(confirmation_result("maybe", true), None);
    assert_eq!(confirmation_result("ny", false), None);
}

#[test]
fn prompt_paste_preserves_unicode_and_shell_characters_but_stays_single_line() {
    let mut input = "prefix ".to_string();

    append_line_paste(&mut input, "Unicode δ; $HOME\r\nnext\tvalue");

    assert_eq!(input, "prefix Unicode δ; $HOMEnextvalue");
}

#[test]
fn disabled_choice_keys_are_not_selectable() {
    let choices = ChoiceList {
        title: "Harness".to_string(),
        choices: vec![
            KeyChoice::disabled("1", "OpenCode"),
            KeyChoice::new("2", "Codex"),
        ],
    };

    assert_eq!(selectable_choice_key(&choices, "1"), None);
    assert_eq!(selectable_choice_key(&choices, "2").as_deref(), Some("2"));
}

#[test]
fn ordered_toggle_groups_enabled_items_before_disabled_items() {
    let mut items = ordered_toggle_items();
    let mut selected = 1;

    toggle_ordered_item(&mut items, &mut selected);

    assert_eq!(selected, 2);
    assert_eq!(
        items
            .iter()
            .map(|item| (item.id.as_str(), item.enabled))
            .collect::<Vec<_>>(),
        vec![("one", true), ("three", false), ("two", false)]
    );

    toggle_ordered_item(&mut items, &mut selected);

    assert_eq!(selected, 1);
    assert_eq!(
        items
            .iter()
            .map(|item| (item.id.as_str(), item.enabled))
            .collect::<Vec<_>>(),
        vec![("one", true), ("two", true), ("three", false)]
    );
}

#[test]
fn ordered_toggle_moves_only_enabled_items() {
    let mut items = ordered_toggle_items();
    let mut selected = 1;

    move_enabled_ordered_item(&mut items, &mut selected, -1);

    assert_eq!(selected, 0);
    assert_eq!(
        items
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        vec!["two", "one", "three"]
    );

    selected = 2;
    move_enabled_ordered_item(&mut items, &mut selected, -1);
    assert_eq!(selected, 2);
}

#[test]
fn recovery_toggle_keeps_stable_order() {
    let mut items = vec![
        OrderedToggleItem {
            id: "first".to_string(),
            label: "First".to_string(),
            enabled: false,
        },
        OrderedToggleItem {
            id: "second".to_string(),
            label: "Second".to_string(),
            enabled: false,
        },
    ];

    toggle_item_in_place(&mut items, 1);

    assert_eq!(items[0].id, "first");
    assert_eq!(items[1].id, "second");
    assert!(!items[0].enabled);
    assert!(items[1].enabled);
}

fn ordered_toggle_items() -> Vec<OrderedToggleItem> {
    vec![
        OrderedToggleItem {
            id: "one".to_string(),
            label: "First".to_string(),
            enabled: true,
        },
        OrderedToggleItem {
            id: "two".to_string(),
            label: "Second".to_string(),
            enabled: true,
        },
        OrderedToggleItem {
            id: "three".to_string(),
            label: "Third".to_string(),
            enabled: false,
        },
    ]
}
