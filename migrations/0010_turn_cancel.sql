alter table sessions add column cancel_turn text;
alter table sessions add column cancel_requested_at timestamptz;
