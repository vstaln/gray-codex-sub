use super::*;

#[test]
fn model_parser_filters_private_and_unsupported_entries() {
    let body = br#"{"models":[
        {"id":"private","display_name":"Private","visibility":"private","supported_in_api":true},
        {"id":"unsupported","display_name":"Unsupported","visibility":"list","supported_in_api":false},
        {"id":"gpt-test","display_name":"GPT Test","visibility":"list","supported_in_api":true,"context_window":128000,"reasoning_efforts":["low","high"]}
    ]}"#;
    let catalog = parse_models(body).unwrap();
    assert_eq!(catalog.models.len(), 1);
    assert_eq!(catalog.models[0].id, "gpt-test");
    assert_eq!(catalog.models[0].context_window, Some(128000));
    assert_eq!(
        catalog.models[0].reasoning_efforts,
        vec!["low".to_string(), "high".to_string()]
    );
}
