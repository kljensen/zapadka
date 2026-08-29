# Formatter fixture provenance

These SQL inputs are verbatim fixtures from the `pgFormatter` PostgreSQL
regression corpus, copied from the upstream paths recorded below. pgFormatter
is distributed under the PostgreSQL License (see its repository LICENSE).

- `pgformatter-comments.sql`: `t/pg-test-files/sql/comments.sql`
  at <https://github.com/darold/pgFormatter/blob/master/t/pg-test-files/sql/comments.sql>
- `pgformatter-create-type.sql`: `t/pg-test-files/sql/create_type.sql`
  at <https://github.com/darold/pgFormatter/blob/master/t/pg-test-files/sql/create_type.sql>

They are exercised as parse → format → parse and idempotence cases. Their
purpose here is to keep Zapadka's libpg_query formatter covered by real-world
PostgreSQL syntax and comment layouts rather than a hand-written toy corpus.
