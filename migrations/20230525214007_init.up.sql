create table if not exists callers (
    id serial primary key,
    phone text not null unique,
    city text,
    state text,
    country text,
    zip text
) ;

create table if not exists calls (
    id serial primary key,
    caller integer not null references callers ( id ),
    twilio_call_sid text not null unique,
    twilio_stream_sid text not null unique,
    dg_request_id uuid not null unique,
    created timestamp with time zone not null default current_timestamp
) ;

create table if not exists turns (
    id serial primary key,
    call integer not null references calls ( id ),
    -- order index of this turn in this call
    call_idx integer not null,
    caller_side text not null,
    bot_side text not null,
    topic text,
    summary text
) ;
