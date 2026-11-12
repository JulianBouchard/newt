alter table sessions
    add column compaction_failures integer not null default 0,
    add column compaction_retry_after timestamptz;
