alter table sessions add column provider_state jsonb;

create table turn_ends (
    id bigserial primary key,
    agent text not null,
    key text not null,
    turn_id text not null,
    payload jsonb not null,
    attempts integer not null default 0,
    next_attempt_at timestamptz not null default now(),
    created_at timestamptz not null default now(),
    foreign key (agent, key) references sessions (agent, key) on delete cascade
);

create index turn_ends_due on turn_ends (next_attempt_at);
