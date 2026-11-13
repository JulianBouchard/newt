create index inbox_consumed on inbox (consumed_at) where consumed_at is not null;
create index live_events_created on live_events (created_at);
drop table outbox;
drop table timers;
