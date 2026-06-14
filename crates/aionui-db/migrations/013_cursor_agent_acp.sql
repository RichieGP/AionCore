-- Cursor CLI now exposes ACP through the `agent` binary.
-- Keep this as a forward migration; migration 001 is already applied in existing installs.
UPDATE agent_metadata
SET
    agent_source_info = '{"binary_name":"agent"}',
    command = 'agent',
    args = '["acp"]',
    behavior_policy = '{"supports_side_question":false,"supports_team":true}',
    yolo_id = 'agent',
    updated_at = unixepoch('now','subsec')*1000
WHERE id = 'a0dfb1ec';
