create table sessions (
    agent text not null,
    key text not null,
    version text,
    lease_owner text,
    lease_epoch bigint not null default 0,
    lease_expires_at timestamptz,
    next_history_seq bigint not null default 0,
    created_at timestamptz not null default now(),
    primary key (agent, key)
);

create table inbox (
    id bigint generated always as identity primary key,
    agent text not null,
    key text not null,
    event_id text not null,
    kind text not null,
    policy text not null,
    priority smallint not null,
    payload jsonb not null,
    received_at timestamptz not null default now(),
    consumed_at timestamptz,
    unique (agent, key, event_id),
    foreign key (agent, key) references sessions (agent, key) on delete cascade
);
create index inbox_pending on inbox (agent, key, id) where consumed_at is null;

create table history (
    agent text not null,
    key text not null,
    seq bigint not null,
    turn_id text not null,
    kind text not null,
    entry jsonb not null,
    tokens integer not null,
    created_at timestamptz not null default now(),
    primary key (agent, key, seq),
    foreign key (agent, key) references sessions (agent, key) on delete cascade
);

create table checkpoints (
    agent text not null,
    key text not null,
    through_seq bigint not null,
    snapshot jsonb not null,
    summary text not null,
    created_at timestamptz not null default now(),
    primary key (agent, key, through_seq),
    foreign key (agent, key) references sessions (agent, key) on delete cascade
);

create table turn_journal (
    agent text not null,
    key text not null,
    turn_id text not null,
    seq integer not null,
    kind text not null,
    payload jsonb not null,
    created_at timestamptz not null default now(),
    primary key (agent, key, turn_id, seq),
    foreign key (agent, key) references sessions (agent, key) on delete cascade
);

create table outbox (
    id bigint generated always as identity primary key,
    agent text not null,
    key text not null,
    kind text not null,
    payload jsonb not null,
    attempts integer not null default 0,
    created_at timestamptz not null default now(),
    delivered_at timestamptz,
    foreign key (agent, key) references sessions (agent, key) on delete cascade
);
create index outbox_pending on outbox (id) where delivered_at is null;

create table timers (
    id bigint generated always as identity primary key,
    agent text not null,
    key text not null,
    due_at timestamptz not null,
    kind text not null,
    payload jsonb not null,
    fired_at timestamptz,
    foreign key (agent, key) references sessions (agent, key) on delete cascade
);
create index timers_due on timers (due_at) where fired_at is null;

create table live_events (
    agent text not null,
    key text not null,
    seq bigint not null,
    event jsonb not null,
    created_at timestamptz not null default now(),
    primary key (agent, key, seq),
    foreign key (agent, key) references sessions (agent, key) on delete cascade
);
