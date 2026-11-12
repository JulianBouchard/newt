alter table sessions add column failed_attempts integer not null default 0;
alter table sessions add column retry_after timestamptz;
