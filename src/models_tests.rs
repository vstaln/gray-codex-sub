use super::*;

#[test]
fn model_row_maps_schema_fields_and_skips_hidden() {
    let hidden = serde_json::json!({
        "id": "m0", "model": "m0", "displayName": "M0", "hidden": true,
        "isDefault": false, "description": "", "defaultReasoningEffort": "low",
        "supportedReasoningEfforts": [],
    });
    assert!(model_row(&hidden).is_none());
    let row = model_row(&serde_json::json!({
        "id": "m1", "model": "gpt-fake", "displayName": "GPT Fake",
        "hidden": false, "isDefault": true, "description": "x",
        "defaultReasoningEffort": "medium",
        "supportedReasoningEfforts": [
            {"description": "d", "reasoningEffort": "low"},
            {"description": "d2", "reasoningEffort": "high"},
        ],
    }))
    .expect("visible row");
    assert_eq!(row.id, "gpt-fake");
    assert_eq!(row.name, "GPT Fake");
    assert_eq!(row.context_window, None);
    assert_eq!(
        row.reasoning_efforts,
        vec!["low".to_string(), "high".to_string()]
    );
    // The slug, not the opaque `id`, is what thread/start takes.
    let slug_named = model_row(&serde_json::json!({
        "id": "opaque", "model": "wire-slug", "displayName": "N",
        "hidden": false, "isDefault": false, "description": "",
        "defaultReasoningEffort": "low", "supportedReasoningEfforts": [],
    }))
    .expect("slug row");
    assert_eq!(slug_named.id, "wire-slug");
}
