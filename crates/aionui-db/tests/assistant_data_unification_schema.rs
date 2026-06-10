use aionui_db::init_database_memory;

#[tokio::test]
async fn migration_creates_assistant_unification_tables_and_keeps_legacy_tables() {
    let db = init_database_memory().await.unwrap();

    let table_names: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name IN (
            'assistant_definitions',
            'assistant_overlays',
            'assistant_preferences',
            'assistants',
            'assistant_overrides'
        ) ORDER BY name",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();

    assert_eq!(
        table_names,
        vec![
            "assistant_definitions".to_string(),
            "assistant_overlays".to_string(),
            "assistant_overrides".to_string(),
            "assistant_preferences".to_string(),
            "assistants".to_string(),
        ]
    );
}

#[tokio::test]
async fn assistant_definition_table_has_expected_default_columns() {
    let db = init_database_memory().await.unwrap();

    let columns: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('assistant_definitions')")
        .fetch_all(db.pool())
        .await
        .unwrap_or_default();

    assert!(
        !columns.is_empty(),
        "assistant_definitions should exist before inspecting columns"
    );

    assert!(columns.iter().any(|name| name == "definition_id"));
    assert!(columns.iter().any(|name| name == "assistant_key"));
    assert!(columns.iter().any(|name| name == "default_model_mode"));
    assert!(columns.iter().any(|name| name == "default_permission_mode"));
    assert!(columns.iter().any(|name| name == "default_skill_ids"));
    assert!(columns.iter().any(|name| name == "default_mcp_ids"));
    assert!(columns.iter().any(|name| name == "avatar_type"));
    assert!(columns.iter().any(|name| name == "avatar_value"));

    let overlay_columns: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('assistant_overlays')")
        .fetch_all(db.pool())
        .await
        .unwrap_or_default();
    assert!(overlay_columns.iter().any(|name| name == "definition_id"));

    let preference_columns: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('assistant_preferences')")
            .fetch_all(db.pool())
            .await
            .unwrap_or_default();
    assert!(preference_columns.iter().any(|name| name == "definition_id"));
}
