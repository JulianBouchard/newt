create function newt_locked_pending_events(session_agent text, session_key text)
returns bigint
language plpgsql
volatile
as $$
begin
    perform pg_advisory_xact_lock(hashtextextended(session_agent || '/' || session_key, 0));
    return (
        select count(*) from inbox
        where agent = session_agent and key = session_key and consumed_at is null
    );
end
$$;
