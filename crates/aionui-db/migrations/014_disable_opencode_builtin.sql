UPDATE agent_metadata
SET enabled = 0,
    updated_at = unixepoch('now','subsec')*1000
WHERE id = '53861a53'
  AND backend = 'opencode'
  AND agent_source = 'builtin';
