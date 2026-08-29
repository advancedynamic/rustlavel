-- The benchmark fixture. Identical for every application under apps/.
drop table if exists bench_posts;
drop table if exists bench_users;

create table bench_users (
    id    integer primary key,
    name  text not null,
    email text not null
);

create table bench_posts (
    id      integer primary key,
    user_id integer not null references bench_users(id),
    title   text not null,
    body    text not null
);

insert into bench_users (id, name, email)
select i, 'User ' || i, 'user' || i || '@example.test'
from generate_series(1, 1000) as i;

insert into bench_posts (id, user_id, title, body)
select i, ((i - 1) % 1000) + 1, 'Post ' || i, repeat('x', 200)
from generate_series(1, 1000) as i;

-- The join in /db/posts is by post id, but the author lookup is by user id.
create index bench_posts_user_id on bench_posts (user_id);
