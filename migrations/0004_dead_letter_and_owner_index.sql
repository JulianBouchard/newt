alter table inbox add column dead_letter text;
create index sessions_lease_owner on sessions (lease_owner) where lease_owner is not null;
